# Stremio Integration — Improvement Backlog

Surfaces beyond the current search → stream → play loop. Ordered by value-for-effort.

Out of scope (decided):
- Meta enrichment for `nzb:` IDs (description/cast/TVDB art/trailers). Keep current minimal meta — videos+streams only — since the search-result detail page depends on it.
- WebDAV. HTTP `Range:` is the right surface for Stremio's player.
- Packed releases (RAR/par2). Single-file NZBs only; `nzb_sanity.rs` is the gate.
- `addon_catalog` resource. Pointless for a private addon.

---

## 1. Stream `behaviorHints` (high value, ~30 min)

Currently `stream::item_to_stream` returns streams without `behaviorHints`. Add:

- **`bingeGroup: "<addonId>-<showId>-<quality>"`** — Stremio auto-plays the next episode using a stream from the same group. Only meaningful for `series`. Key by IMDb id + resolution bucket from `quality.rs` so a user who started in 1080p stays in 1080p across episodes.
- **`videoSize: <bytes>`** — shown in the stream picker. Already known from indexer response.
- **`filename: "<release name>"`** — shown next to size. Already known.
- **`notWebReady: true`** — set on every stream. Tells the web player not to attempt direct play of NNTP-pulled mkv.

Touchpoints: `stream.rs` (`item_to_stream`), `stremio.rs` (extend `Stream` struct + serde flatten on `behaviorHints`).

## 2. Subtitles resource (high value, ~2h)

Add `defineSubtitlesHandler`-equivalent: `/subtitles/{type}/{id}.json`.

- Manifest: `ManifestResource::Detailed { name: "subtitles", types: ["movie","series"], id_prefixes: Some(vec!["tt"]) }`.
- Source: OpenSubtitles or Subdl, keyed by IMDb id (+ season/episode for series — Stremio sends `tt1234567:5:14`).
- Response: `{ subtitles: [{ id, url, lang }] }`.
- Cache responses (subs change rarely).

Stremio queries every subtitle-capable addon at playback start; results merge in the player's subtitle picker.

## 3. More catalogs (Discover rows) (medium value)

Today: one catalog (`tv` + `search`). Stremio renders **each catalog as its own row** on Board + Discover.

Cheap additions, all backed by indexer queries you already make:

- **Recent Movies** — `nzb_api` recent query, `type: "movie"`.
- **Recent Episodes** — same for `type: "series"`.
- **Top 4K** / **Top HD** — quality-bucketed via `quality.rs`.

Each is a `ManifestCatalog` entry. Reuse the candidate→stream pipeline, but return previews (not full meta) so the rows render fast. Add `extra: [{ name: "skip" }]` for pagination.

These return `tt`-prefixed previews when an IMDb id can be resolved from the release name (so click flows through Cinemeta), or `nzb:`-prefixed otherwise (current path).

## 4. Catalog `extra` filters (medium value)

The search catalog only declares `search`. Stremio's UI generates filter chips for whatever's listed:

- `{ name: "genre", options: [...] }`
- `{ name: "year" }`
- `{ name: "quality", options: ["2160p","1080p","720p"] }` — maps to `quality.rs` buckets
- `{ name: "skip" }` — pagination

Already partially handled — `catalog_route_extra` in `main.rs:250` parses `key=value&...`. Just declare the extras in the manifest and switch on them.

## 5. Series episode parser hardening (medium value)

Stremio sends `tt0903747:5:14` for "Breaking Bad S05E14". `parse_title.rs` needs to match release names tightly against `(season, episode)`:

- `S05E14`, `s5e14`
- `5x14`, `05x14`
- `Season 5 Episode 14`
- `5.14`, `S05.E14`
- Date-based dailies: `2024.03.15` for talk shows

Loose matching causes wrong-episode hits; strict matching causes missed streams. Audit + tests.

## 6. Manifest cleanup (low value, fast)

- Review `types: ["movie","series","tv"]` — is `tv` vestigial or used by the search catalog as a parking namespace? If only the search catalog needs it, document it; otherwise drop.
- `id_prefixes` for stream resource: confirm `nzb` prefix is still hit by anything besides the search-result expansion path.

---

## Picking order

If we're committing to a sprint:

1. `bingeGroup` + `videoSize` + `filename` + `notWebReady` (#1) — series UX transformation, half a day.
2. Subtitles (#2) — removes the most common "stream is broken" complaint.
3. Episode parser hardening (#5) — quality-of-streams improvement, shows up everywhere.
4. Extra catalogs + filters (#3, #4) — Discover surface, biggest visible change.
5. Manifest cleanup (#6) — alongside any of the above.
