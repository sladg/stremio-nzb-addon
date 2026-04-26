# Tabellarius

A Stremio addon that pulls streams from Newznab indexers (Drunken Slug, NZBGeek, nzbplanet, …), validates results against your NNTP provider, and plays releases straight inside Stremio over HTTP — no full download to disk first.

From Stremio's perspective it's a normal HTTP stream addon — all the NNTP/yEnc/RAR work happens server-side, so you don't need any experimental Stremio flags or builds.

> _Designed by human, coded by Claude._

## What it does

- **Multi-indexer search** — any Newznab API works; all configured indexers are queried in parallel and results merged.
- **Pre-flight validation** — optional NZB structure check (RAR filename scan) and live NNTP article probe, so dead or missing releases are filtered before Stremio sees them.
- **Quality gates** — bandwidth window in Gbit/h plus a regex blocklist for unwanted release types.
- **Language preferences** — ISO codes, full names, or the `original` token (resolves the title's production country to its language via Cinemeta — Korean show → Korean, German movie → German, etc.).
- **Flat and RARed releases** — bare video NZBs plus uncompressed multi-volume RARs stream directly; compressed or encrypted RARs are rejected at pre-flight.
- **HTTP range streaming** — segments are fetched on demand from NNTP, decoded, and served as HTTP byte-range responses. Supports seek/scrub. Sparse disk cache with configurable size cap; idle sessions evicted automatically.
- **Multi-server NNTP failover** — each segment is tried against all configured NNTP servers in order; playback continues if one server is missing an article.
- **Multi-user with access keys** — share with friends without exposing your indexer or NNTP credentials. Each user gets their own URL key with per-user overrides for languages, quality gates, and indexers. `requireAuth = true` refuses to boot with an empty user list.

## Quick start

```sh
git clone https://github.com/sladg/stremio-tabellarius-addon.git
cd stremio-tabellarius-addon

# Edit the addon_config block at the bottom of docker-compose.yml:
#   - replace REPLACE_WITH_* with real indexer + NNTP credentials
#   - replace the access key with your own (`openssl rand -hex 16`)
$EDITOR docker-compose.yml

docker compose up -d
docker compose logs -f addon   # check it started cleanly
```

## Connect Stremio

In Stremio, open **Add-ons**, paste this URL into the "Add-on Repository URL" / install field, and click **Install**:

```
http://<host>:3000/<access-key>/manifest.json
```

`<host>` is `localhost` if Stremio runs on the same machine, otherwise your server's IP or domain. `<access-key>` is the `key = "..."` value you set under `[users.<name>]`.

Once installed, open any movie or TV show — Tabellarius streams appear in the Streams tab alongside any other addons you have configured (Torrentio, Cinemeta, etc.). Click one to play.

## Configuration

**Everything you configure lives in `config.toml`** — indexers, NNTP servers, users, filters, language preferences. The fully annotated reference (every field, every option, with examples) is in [`config.example.toml`](./config.example.toml).

For Docker users, `config.toml` is inlined as the `addon_config` block at the bottom of [`docker-compose.yml`](./docker-compose.yml). Edit it there and re-run `docker compose up -d` to apply.

Three sections matter:

- `[defaults]` — applied to every request: quality window, language preferences, validation toggles, exclusion regex.
- `[[defaults.indexers]]` / `[[defaults.nntpServers]]` — at least one of each is required.
- `[users.<name>]` — each user has a friendly map key (used in logs) and a `key = "..."` URL secret. The Stremio install URL becomes `/{key}/manifest.json`. Per-user fields override `[defaults]`.

Generate access keys with `openssl rand -hex 16`.

### Advanced: runtime env vars (optional)

`config.toml` covers the addon's behaviour. Env vars are a secondary surface for runtime knobs (cache size, abuse protection, networking) — defaults are sensible, most deployments don't touch them.

| Var | Default | Purpose |
| --- | --- | --- |
| `CACHE_BYTES` | `1073741824` (1 GiB) | streaming cache size cap |
| `CACHE_DIR` | `/cache` | streaming cache path |
| `IDLE_TIMEOUT_SECS` | `3600` | idle-session GC threshold |
| `PROTECT_WINDOW_SECS` | `300` | recently-active sessions immune from cap-eviction |
| `RATE_LIMIT_PER_MINUTE` | `60` | per-IP request limit (0 disables) |
| `RATE_LIMIT_BURST` | `30` | leaky-bucket burst size |
| `BAN_FAILURE_THRESHOLD` | `5` | bad-token rejects before IP ban (0 disables) |
| `BAN_WINDOW_SECS` | `300` | ban detection window |
| `BAN_DURATION_SECS` | `3600` | ban duration |
| `TRUST_PROXY_HEADERS` | unset | set `1` when behind a reverse proxy that strips inbound `X-Forwarded-For` |
| `BIND_ADDR` | `0.0.0.0` | listen address inside the container |
| `PORT` | `3000` | listen port |
| `CONFIG_PATH` | `/config.toml` | config file path |
| `RUST_LOG` | `info` | tracing filter |

## Public deployments

If you're exposing the addon beyond your LAN, treat the access keys as bearer tokens — they sit in the URL path and will appear in any HTTP access log along the way.

- **Always run behind TLS.** Path tokens are visible in plaintext over HTTP and in reverse-proxy access logs.
- **Don't publish port 3000 to the internet.** Bind the host port to localhost only — change `ports: "3000:3000"` to `ports: "127.0.0.1:3000:3000"` in `docker-compose.yml`, and put Caddy / Cloudflare Tunnel / nginx in front of it for TLS termination.
- **Keep `requireAuth = true`** (default in `config.example.toml`). Boot fails if `[users]` is empty — protects you from accidentally shipping an unauthenticated addon.
- **Set `TRUST_PROXY_HEADERS=1` only** when your proxy strips the inbound copies of `X-Forwarded-For` / `X-Real-IP`. Otherwise the IP ban / rate limiter sees client-spoofed addresses.

## New to Usenet?

You need two things:

1. **An indexer** — a search service that returns NZB files (release metadata pointing at NNTP articles). Any Newznab-compatible indexer works; popular options include Drunken Slug, NZBGeek, and nzbplanet.
2. **A provider** — the actual NNTP server that stores and serves the binary segments. Commercial options include Easynews, Newshosting, UsenetServer, and Eweka. You'll get a host, port, username, and password.

Plug both into `docker-compose.yml` and you're set. Higher provider retention = older content stays available.

## License

[MIT](./LICENSE)
