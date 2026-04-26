import { Request, Response } from "express";
import { connect as tlsConnect } from "tls";
import { connect as netConnect } from "net";

interface HealthcheckResponse {
  ok: boolean;
  error?: string;
}

/** Test Newznab/NZBHydra API connectivity via caps endpoint */
export const testIndexerHealth = async (
  url: string,
  apiKey: string,
): Promise<HealthcheckResponse> => {
  try {
    const capsUrl = new URL(url);
    capsUrl.pathname = "/api";
    capsUrl.searchParams.set("t", "caps");
    capsUrl.searchParams.set("apikey", apiKey);

    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), 5000);

    const response = await fetch(capsUrl.toString(), {
      signal: controller.signal,
    });
    clearTimeout(timeoutId);

    if (!response.ok) {
      return {
        ok: false,
        error: `HTTP ${response.status}: ${response.statusText}`,
      };
    }

    // Verify response is valid
    const contentType = response.headers.get("content-type");
    if (contentType?.includes("application/json")) {
      await response.json();
    } else if (contentType?.includes("xml")) {
      await response.text();
    }

    return { ok: true };
  } catch (error) {
    if (error instanceof Error) {
      return {
        ok: false,
        error:
          error.name === "AbortError"
            ? "Connection timeout (5s)"
            : error.message,
      };
    }
    return { ok: false, error: "Unknown error" };
  }
};

/** Test NNTP server connectivity and authentication */
export const testNntpHealth = (
  serverUrl: string,
): Promise<HealthcheckResponse> =>
  new Promise((resolve) => {
    try {
      const url = new URL(serverUrl);
      const isSecure = url.protocol === "nntps:";
      const host = url.hostname;
      const port = url.port ? parseInt(url.port) : isSecure ? 563 : 119;
      const username = decodeURIComponent(url.username);
      const password = decodeURIComponent(url.password);

      let authenticated = false;
      let buffer = "";

      const timeout = setTimeout(() => {
        socket?.destroy();
        resolve({ ok: false, error: "Connection timeout (5s)" });
      }, 5000);

      const connectOpts = {
        host,
        port,
        ...(isSecure && { rejectUnauthorized: false }),
      };
      const socket = isSecure
        ? tlsConnect(connectOpts)
        : netConnect(connectOpts);

      socket.on("data", (data: Buffer) => {
        buffer += data.toString();
        const lines = buffer.split("\r\n");
        buffer = lines.pop() || "";

        for (const line of lines) {
          const code = parseInt(line.substring(0, 3));

          if ((code === 200 || code === 201) && !authenticated) {
            if (username && password) {
              socket.write(`AUTHINFO USER ${username}\r\n`);
            } else {
              clearTimeout(timeout);
              socket.destroy();
              resolve({ ok: true });
            }
          } else if (code === 381) {
            socket.write(`AUTHINFO PASS ${password}\r\n`);
          } else if (code === 281) {
            authenticated = true;
            clearTimeout(timeout);
            socket.destroy();
            resolve({ ok: true });
          } else if (code >= 480 && code < 490) {
            clearTimeout(timeout);
            socket.destroy();
            resolve({ ok: false, error: `Authentication failed: ${line}` });
          } else if (code >= 500) {
            clearTimeout(timeout);
            socket.destroy();
            resolve({ ok: false, error: `Server error: ${line}` });
          }
        }
      });

      socket.on("error", (err) => {
        clearTimeout(timeout);
        resolve({ ok: false, error: err.message });
      });

      socket.on("close", () => {
        clearTimeout(timeout);
        if (!authenticated && username && password) {
          resolve({ ok: false, error: "Connection closed before auth" });
        }
      });
    } catch (error) {
      resolve({
        ok: false,
        error: error instanceof Error ? error.message : "Unknown error",
      });
    }
  });

/** Express handler for indexer healthcheck */
export const indexerHealthcheckHandler = async (
  req: Request,
  res: Response,
) => {
  const { url, apiKey } = req.body;

  if (!url || !apiKey) {
    return res.status(400).json({ ok: false, error: "Missing url or apiKey" });
  }

  const result = await testIndexerHealth(url, apiKey);
  res.json(result);
};

/** Express handler for NNTP healthcheck */
export const nntpHealthcheckHandler = async (req: Request, res: Response) => {
  const { server } = req.body;

  if (!server) {
    return res.status(400).json({ ok: false, error: "Missing server" });
  }

  const result = await testNntpHealth(server);
  res.json(result);
};
