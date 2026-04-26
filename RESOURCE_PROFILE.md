# Resource Profile

Worked sizing for the streaming pipeline at the current defaults. All figures assume the deployment in `homelab/apps/stremio.yaml`:

- `READ_AHEAD_SEGMENTS=8` (per stream)
- `CACHE_HEADER_PIN_BYTES=64 MiB`
- `CACHE_BACKBUFFER_BYTES=1 GiB`
- `CACHE_EVICT_STEP_BYTES=256 MiB`
- `CACHE_BYTES=10 GiB` (real-bytes accounted via `st_blocks*512`)
- `emptyDir sizeLimit=15 GiB` (real-bytes ceiling on the node)
- 2 NNTP servers, each `/20` connections (40 total pool slots)
- Pod limits: `4 GiB` mem / `4 vCPU`

---

## Constants used in the math

| Quantity | Value | Source |
|---|---|---|
| Avg NNTP segment size (decoded) | ~750 KiB | Typical Usenet posters |
| Per-stream throughput | ~15–20 MB/s | Read-ahead at N=8, pool-bound |
| yEnc decode cost | ~0.4 vCPU per active stream | Measured |
| Decode + write working set per in-flight segment | ~1.5 MiB | Raw + decoded buffer |

**Per-stream RAM model:**

```
mem(stream) = read_ahead_segments × ~1.5 MiB        # in-flight decode buffers
            + ~5 MiB                                  # tokio task overhead, async-stream state
            + ~256 KiB                                # foreground yield chunk (CHUNK_SIZE)
```

So at `READ_AHEAD_SEGMENTS=8`: `≈ 8 × 1.5 + 5 + 0.25 = ~17 MiB working set per stream`. Plus the `tokio` / `moka` / `NntpPool` baseline (~200 MiB process-wide, mostly fixed).

**Per-stream disk model:**

The cache file is sparse-allocated to the full video size at session-open (logical), but real disk usage is bounded by sliding-window eviction. As the playhead advances past `header_pin + backbuffer`, bytes behind it are punched out of the file via `fallocate(PUNCH_HOLE)` so real on-disk usage settles at:

```
disk_real(stream) ≈ header_pin + backbuffer + ~1 step
                  = 64 MiB + 1 GiB + 256 MiB
                  ≈ 1.32 GiB
```

regardless of whether the video is 4 GiB or 50 GiB. The 10 GiB `CACHE_BYTES` cap is checked against real bytes (`st_blocks*512`), so it reflects actual disk pressure.

---

## Scenario 1 — 1× 720p (4 GiB, ~2 h, ~4.5 Mbps)

The "comfort zone" — fits with enormous headroom on every axis.

| Resource | Value | Limit | Headroom |
|---|---|---|---|
| RAM (working set) | ~17 MiB | 4 GiB | ✅ enormous |
| Disk (real, end of playback) | ~1.06 GiB | 15 GiB emptyDir | ✅ 14 GiB free |
| NNTP conns in flight | 8 | 40 pool | ✅ 32 idle |
| Bitrate vs. throughput | 4.5 Mbps / ~120+ Mbps | — | ✅ 25× over |

---

## Scenario 2 — 1× 1080p WEB-DL (10 GiB, ~2 h, ~11 Mbps)

| Resource | Value | Limit | Headroom |
|---|---|---|---|
| RAM (working set) | ~17 MiB | 4 GiB | ✅ |
| Disk (real, end of playback) | ~1.06 GiB | 15 GiB emptyDir | ✅ |
| NNTP conns in flight | 8 | 40 | ✅ |
| Bitrate vs. throughput | 11 Mbps / ~120+ Mbps | — | ✅ 10× over |

---

## Scenario 3 — 1× 4K UHD remux (20 GiB, ~2 h, ~22 Mbps)

The original problem case ("bigger files = buffering"). Now comfortable.

| Resource | Value | Limit | Headroom |
|---|---|---|---|
| RAM (working set) | ~17 MiB | 4 GiB | ✅ |
| Disk (real, end of playback) | ~1.06 GiB | 15 GiB emptyDir | ✅ — was 20 GiB pre-eviction |
| NNTP conns in flight | 8 | 40 | ✅ |
| Bitrate vs. throughput | 22 Mbps / ~120+ Mbps | — | ✅ ~5× over |

Pre-read-ahead, ~22 Mbps content vs the ~24 Mbps pipeline ceiling meant any RTT spike → rebuffer. Both fixes (read-ahead + eviction) compound here: the 20 GiB sparse file no longer fills the emptyDir, and the pipeline runs ~5× faster than the bitrate.

---

## Scenario 4 — 3× concurrent mixed (1× 4K + 2× 1080p)

Realistic "Saturday night" load: someone on the TV, two friends on phones.

| Resource | Per-stream | Total (3 streams) | Limit | Headroom |
|---|---|---|---|---|
| RAM (working set) | ~17 MiB | ~50 MiB + 200 MiB baseline | 4 GiB | ✅ |
| Disk (real, mid-playback) | ~1.06 GiB | ~3.2 GiB | 15 GiB emptyDir | ✅ |
| NNTP conns in flight | 8 | 24 | 40 | ✅ 16 idle |
| Aggregate bandwidth | ~20 MB/s each | ~60 MB/s total | provider bw | ⚠️ depends on provider |
| CPU (yEnc decode) | ~0.4 vCPU | ~1.2 vCPU + bursts | 4 vCPU | ✅ |

---

## Scenario 5 — 5× concurrent 4K UHD remuxes (worst case sustained)

Stress-test scenario, included for completeness.

| Resource | Per-stream | Total (5 streams) | Limit | Headroom |
|---|---|---|---|---|
| RAM (working set) | ~17 MiB | ~85 MiB + 200 MiB baseline | 4 GiB | ✅ |
| Disk (real, mid-playback) | ~1.06 GiB | ~5.3 GiB | 15 GiB emptyDir | ✅ |
| NNTP conns in flight | 8 | 40 | 40 | ⚠️ pool fully saturated |
| Aggregate bandwidth | ~20 MB/s each | ~100 MB/s | provider bw | ❌ likely throttled |
| CPU (yEnc decode) | ~0.4 vCPU | ~2.0 vCPU + bursts to ~4.5 | 4 vCPU | ⚠️ at ceiling |

Disk and RAM are now non-issues at this scale. What still binds:

1. **NNTP pool**: 8 read-ahead × 5 streams = 40 (= pool size). Any failover spills queue. Drop `READ_AHEAD_SEGMENTS` to 4 if 5+ concurrent is normal — per-stream throughput drops to ~10 MB/s, still 5× a 4K remux's bitrate.
2. **CPU**: bursts near ceiling on RAR re-seeks. Bump pod limits to 8 vCPU if regular.
3. **Provider bandwidth**: 100 MB/s sustained is more than typical block accounts will give cheaply.

In practice: cap concurrent streams at 3 via Tailscale ACLs or a simple in-pod gate.

---

## Cross-scenario summary

| Scenario | Streams | RAM | Disk real | NNTP conns | Verdict |
|---|---|---|---|---|---|
| 1 — 720p single | 1 | ~17 MiB | ~1.06 GiB | 8 / 40 | ✅ trivial |
| 2 — 1080p single | 1 | ~17 MiB | ~1.06 GiB | 8 / 40 | ✅ trivial |
| 3 — 4K UHD single | 1 | ~17 MiB | ~1.06 GiB | 8 / 40 | ✅ comfortable |
| 4 — 3× mixed concurrent | 3 | ~250 MiB | ~3.2 GiB | 24 / 40 | ✅ comfortable |
| 5 — 5× 4K UHD concurrent | 5 | ~285 MiB | ~5.3 GiB | 40 / 40 | ⚠️ pool saturated, CPU near ceiling |

**The streaming pipeline scales cleanly to 3× concurrent at any resolution.** Beyond that, NNTP pool size and CPU become the binding constraints — disk and RAM stay flat thanks to sliding-window eviction.

What's left to be aware of:

- **Seek-back beyond 1 GiB backbuffer triggers refetch** (~6 min of 4K / ~30 min of 1080p of slack). Bump `CACHE_BACKBUFFER_BYTES` if your players seek aggressively across hours.
- **`READ_AHEAD_SEGMENTS=8` × 5+ streams ≥ pool size**. Drop to 4 if regularly running 5+ concurrent.

---

## Sizing knobs at a glance

| Knob | Default | Trade-off |
|---|---|---|
| `READ_AHEAD_SEGMENTS` | 8 | Higher = more throughput up to pool capacity, more in-flight RAM. Lower = less throughput, fewer NNTP conns used. |
| `CACHE_HEADER_PIN_BYTES` | 64 MiB | Higher = more disk locked, bulletproof against any container quirk. Lower = save disk if you trust your media. |
| `CACHE_BACKBUFFER_BYTES` | 1 GiB | Higher = more aggressive scrub-back without refetch. Lower = smaller per-stream footprint. Set to `0` to disable eviction entirely. |
| `CACHE_EVICT_STEP_BYTES` | 256 MiB | Higher = fewer fallocate syscalls, evicted bytes linger longer. Lower = more responsive but more syscall overhead. |
| `CACHE_BYTES` | 10 GiB | Total real-bytes cap across all sessions. GC evicts oldest non-active sessions when over. |

---

## Indexer load (separate concern from streaming)

Streaming and indexer activity are decoupled, but every Stremio search and playback click talks to the indexer (e.g. nzbplanet, nzbgeek). Most providers cap free/lifetime accounts at **2000 .nzb downloads/day** even when their dashboard shows "Unlimited" — and an unoptimized addon can burn through that in a few hours of casual browsing. Three improvements landed alongside the streaming work to reduce per-search indexer load:

### 1. Pipeline reorder (cap before validate)

The per-resolution cap (e.g. `streamsPerResolution = 2`) now runs *before* `nzb_sanity::filter_by_nzb_sanity`, on a 2× buffer. So a search that previously sent ~20–30 candidates through `getnzb` now sends ~6–8. Net **~3× reduction in indexer hits per search** under normal load.

### 2. Shared NZB-bytes cache + retry with backoff

`fetch_nzb_xml` (in `nzb_fetch.rs`) is the single fetch path for both `nzb_sanity` (during search listing) and `streaming::preflight` (when the user clicks play). Successful XML payloads are cached in moka (64 MiB byte-weighted, 7-day TTL) so the click-time preflight reuses sanity-time bytes instead of refetching. Failed attempts retry up to 3× with 200 ms / 600 ms backoff for 5xx and network errors; 4xx is treated as deterministic (no retry).

Result: **playback-click cost drops from 1 indexer hit to 0** for any title where sanity has already cached the bytes.

### 3. Per-host throttle on indexer 5xx

When `getnzb` returns 5xx, the host enters a 5-minute cooldown. Subsequent `fetch_nzb_xml` calls for that host short-circuit to `IndexerThrottled` without touching the network. So a single search that finds the indexer capped doesn't cascade into 30+ wasted retries across follow-up searches.

### Combined effect on indexer quota burn

| Scenario | Pre-changes | Post-changes |
|---|---|---|
| Single search, indexer healthy | ~20 `getnzb` calls | ~6–8 calls |
| Single search, indexer 503'ing | ~60 retry calls | ~24 calls (1st batch parallel) → throttle armed |
| Follow-up search within 5 min, throttle armed | ~60 retry calls | **0 calls** |
| Click play, sanity bytes cached | 1 preflight `getnzb` call | **0 calls** (bytes-cache hit) |
| Re-watch same title within 7 days | 1 search call | **0 calls** if RSS cache warm (12h TTL) |

Cache TTLs (in `cache.rs` and `nzb_fetch.rs`):

| Cache | TTL | Notes |
|---|---|---|
| `RSS_CACHE` (search results) | 12 h | newer uploads churn the first hour or two |
| `SANITY_CACHE` (verdicts) | 7 d | RAR-vs-Flat structure is immutable once posted |
| `NZB_BYTES_CACHE` (XML bodies) | 7 d | article-ID lists are immutable once posted |
| `AVAILABILITY_CACHE` (NNTP probe) | 24 h | bound by provider article retention |
| `INDEXER_THROTTLE` | 5 min | per-host cooldown after 5xx |

### Long-term: multi-indexer fallback

The strongest defense against any single indexer's quota or downtime is having two configured. The addon already supports `[[defaults.indexers]]` with multiple blocks — `nzb_api::call` fans out across them. Pair nzbplanet with nzbgeek or drunkenslug (~$10–15/lifetime each) and a single account hitting its cap stops being a user-visible event.

---

## What changed (historical)

For context — what these numbers looked like before the recent work:

| Axis | Before | After |
|---|---|---|
| Per-stream throughput | ~3 MB/s sequential | ~15–20 MB/s read-ahead |
| Disk (real) for a 20 GiB stream at end-of-playback | ~20 GiB | ~1.06 GiB (sliding-window eviction) |
| Cache cap accounting | logical (`metadata.len()`) | real (`st_blocks*512`) |
| Indexer hits per search | ~20+ | ~6–8 (pre-validation cap) |
| Indexer hits on cap'd indexer (follow-up search) | 60+ retries | 0 (per-host throttle) |
| Click-to-play indexer hits | 1 (refetch) | 0 (bytes cache shared) |

Disk is no longer the binding constraint at any realistic concurrency. Pipeline throughput and indexer quota burn both improved by an order of magnitude. The remaining levers are NNTP pool size and CPU at 5+ concurrent streams.
