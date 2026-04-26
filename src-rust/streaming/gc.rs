//! Background GC for stream sessions and their cache files.
//!
//! Runs every `interval` seconds. On each tick:
//!   1. Drops sessions idle past `idle_timeout`. Their cache files are deleted.
//!   2. If total cache disk usage > `max_bytes`, evicts oldest-accessed sessions
//!      (skipping ones touched within `protect_window`) until under cap.
//!
//! The protect window prevents the cap-evictor from killing a session whose
//! Stremio client is mid-playback.

use std::path::Path;
use std::time::Duration;

use crate::streaming::session::{now_ms, SessionRegistry};

#[derive(Debug, Clone, Copy)]
pub struct GcConfig {
    /// How often to run GC. Default 60 s.
    pub interval: Duration,
    /// Sessions idle this long get evicted regardless of disk usage. Default 1 h.
    pub idle_timeout: Duration,
    /// Total disk-budget for all cache files. Default 1 GiB.
    pub max_bytes: u64,
    /// Sessions accessed within this window are protected from cap-eviction
    /// (= "probably playing right now"). Default 5 min.
    pub protect_window: Duration,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(3600),
            max_bytes: 1024 * 1024 * 1024,
            protect_window: Duration::from_secs(300),
        }
    }
}

/// Spawn the GC task. Returns immediately.
pub fn spawn(registry: SessionRegistry, cache_root: std::path::PathBuf, config: GcConfig) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(config.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Skip the immediate first tick — let the server warm up.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            run_once(&registry, &cache_root, &config).await;
        }
    });
}

/// One GC pass. Public for tests.
pub async fn run_once(registry: &SessionRegistry, cache_root: &Path, config: &GcConfig) {
    let now = now_ms();
    let idle_ms = config.idle_timeout.as_millis() as u64;
    let protect_ms = config.protect_window.as_millis() as u64;

    // 1. Idle eviction.
    let mut evict_idle: Vec<String> = Vec::new();
    for entry in registry.iter() {
        let last = entry.value().last_access_ms();
        if now.saturating_sub(last) >= idle_ms {
            evict_idle.push(entry.key().clone());
        }
    }
    for token in &evict_idle {
        evict_session(registry, cache_root, token).await;
    }
    if !evict_idle.is_empty() {
        tracing::info!("[gc] idle-evicted {} session(s)", evict_idle.len());
    }

    // 2. Cap eviction. Sum file sizes on disk; if over cap, evict oldest
    //    sessions (skipping those in the protect window) until under cap.
    let total = total_disk_bytes(cache_root).await;
    if total <= config.max_bytes {
        return;
    }

    // Collect (last_access, token) for unprotected sessions, sorted ascending.
    let mut candidates: Vec<(u64, String)> = registry
        .iter()
        .filter_map(|e| {
            let last = e.value().last_access_ms();
            if now.saturating_sub(last) < protect_ms {
                None // recently used → protect
            } else {
                Some((last, e.key().clone()))
            }
        })
        .collect();
    candidates.sort_by_key(|(t, _)| *t);

    let mut over = total.saturating_sub(config.max_bytes);
    let start_total = total;
    let mut evicted = 0usize;
    for (_, token) in candidates {
        if over == 0 {
            break;
        }
        let path = cache_root.join(format!("{token}.bin"));
        let bytes = file_bytes(&path).await;
        evict_session(registry, cache_root, &token).await;
        evicted += 1;
        over = over.saturating_sub(bytes);
    }
    if evicted > 0 {
        tracing::info!(
            "[gc] cap-evicted {evicted} session(s); usage {} -> ~{} (cap {})",
            start_total,
            start_total.saturating_sub(start_total - over.min(start_total)),
            config.max_bytes,
        );
    } else if total > config.max_bytes {
        tracing::warn!(
            "[gc] over cap by {} bytes but no evictable sessions (all in protect window)",
            total - config.max_bytes
        );
    }
}

async fn evict_session(registry: &SessionRegistry, cache_root: &Path, token: &str) {
    registry.remove(token);
    let path = cache_root.join(format!("{token}.bin"));
    if let Err(err) = tokio::fs::remove_file(&path).await {
        if err.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("[gc] failed to delete cache file {}: {err}", path.display());
        }
    }
}

/// Sum of *real* cache-file disk usage under `cache_root`. Counts allocated
/// blocks (not logical sparse size), so cache files that are large but
/// mostly hole-punched count what they actually consume on disk. This is
/// what makes the `max_bytes` cap meaningful once sliding-window eviction
/// is active. Also exposed for `/api/status`.
pub async fn total_disk_bytes(cache_root: &Path) -> u64 {
    let Ok(mut rd) = tokio::fs::read_dir(cache_root).await else {
        return 0;
    };
    let mut total = 0u64;
    while let Ok(Some(entry)) = rd.next_entry().await {
        if let Ok(meta) = entry.metadata().await {
            if meta.is_file() {
                total += real_bytes(&meta);
            }
        }
    }
    total
}

async fn file_bytes(path: &Path) -> u64 {
    tokio::fs::metadata(path)
        .await
        .map(|m| real_bytes(&m))
        .unwrap_or(0)
}

/// Real on-disk usage from a `Metadata`. On Unix, `st_blocks * 512` (POSIX
/// guarantees 512-byte blocks for `st_blocks` regardless of the
/// filesystem's block size). On other platforms, falls back to logical
/// `len()` — only matters for macOS dev, prod is Linux.
fn real_bytes(meta: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.blocks() * 512
    }
    #[cfg(not(unix))]
    {
        meta.len()
    }
}

/// Clear all `.bin` files in the cache directory. Called at boot to reap
/// orphans from the previous run (sessions are in-memory only).
pub async fn clear_cache_dir(cache_root: &Path) -> std::io::Result<usize> {
    let mut rd = tokio::fs::read_dir(cache_root).await?;
    let mut removed = 0;
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("bin")
            && tokio::fs::remove_file(&path).await.is_ok()
        {
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::candidate::{candidate_from_item, NzbCandidate};
    use crate::streaming::session::{new_registry, register_group};
    use std::sync::atomic::Ordering;

    fn dummy_candidate() -> NzbCandidate {
        candidate_from_item(crate::nzb_api::Item {
            title: "Movie.2024.1080p.WEB-DL.x265-RARBG".to_string(),
            ..Default::default()
        })
    }

    fn tempdir() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nzb-gc-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn idle_eviction_drops_old_sessions() {
        let registry = new_registry();
        let cache_root = tempdir();
        let token = register_group(&registry, vec![dummy_candidate()]);
        // Backdate last_access by 2 hours.
        registry
            .get(&token)
            .unwrap()
            .last_access
            .store(now_ms() - 7200_000, Ordering::Relaxed);
        // Create a fake cache file.
        std::fs::write(cache_root.join(format!("{token}.bin")), b"x").unwrap();

        let cfg = GcConfig {
            idle_timeout: Duration::from_secs(3600),
            ..GcConfig::default()
        };
        run_once(&registry, &cache_root, &cfg).await;

        assert!(registry.get(&token).is_none());
        assert!(!cache_root.join(format!("{token}.bin")).exists());
        std::fs::remove_dir_all(&cache_root).ok();
    }

    #[tokio::test]
    async fn cap_eviction_drops_oldest_first() {
        let registry = new_registry();
        let cache_root = tempdir();
        let now = now_ms();

        // Three sessions with staggered last_access. Each backed by a 1 MiB
        // cache file. Cap = 1 MiB → must evict 2 to fit.
        let mut tokens = Vec::new();
        for i in 0..3 {
            let token = register_group(&registry, vec![dummy_candidate()]);
            // Old: 30 min ago, mid: 20 min ago, new: 10 min ago.
            // All past the 5 min protect window, so all eligible.
            let age_ms = (30 - i * 10) * 60 * 1000;
            registry
                .get(&token)
                .unwrap()
                .last_access
                .store(now - age_ms as u64, Ordering::Relaxed);
            std::fs::write(
                cache_root.join(format!("{token}.bin")),
                vec![0u8; 1024 * 1024],
            )
            .unwrap();
            tokens.push(token);
        }

        let cfg = GcConfig {
            max_bytes: 1024 * 1024,
            idle_timeout: Duration::from_secs(86400), // long, so idle path doesn't fire
            protect_window: Duration::from_secs(300),
            ..GcConfig::default()
        };
        run_once(&registry, &cache_root, &cfg).await;

        // Newest survives; older two evicted.
        assert!(
            registry.get(&tokens[0]).is_none(),
            "oldest should be evicted"
        );
        assert!(
            registry.get(&tokens[1]).is_none(),
            "middle should be evicted"
        );
        assert!(registry.get(&tokens[2]).is_some(), "newest should survive");

        std::fs::remove_dir_all(&cache_root).ok();
    }

    #[tokio::test]
    async fn protect_window_keeps_recent_sessions() {
        let registry = new_registry();
        let cache_root = tempdir();
        let now = now_ms();

        // One session, 10 MiB cache file, accessed 30 s ago. Cap = 1 MiB
        // (way over). But it's inside the 5-min protect window → kept.
        let token = register_group(&registry, vec![dummy_candidate()]);
        registry
            .get(&token)
            .unwrap()
            .last_access
            .store(now - 30_000, Ordering::Relaxed);
        std::fs::write(
            cache_root.join(format!("{token}.bin")),
            vec![0u8; 10 * 1024 * 1024],
        )
        .unwrap();

        let cfg = GcConfig {
            max_bytes: 1024 * 1024,
            idle_timeout: Duration::from_secs(86400),
            protect_window: Duration::from_secs(300),
            ..GcConfig::default()
        };
        run_once(&registry, &cache_root, &cfg).await;

        assert!(
            registry.get(&token).is_some(),
            "protected session must survive"
        );
        std::fs::remove_dir_all(&cache_root).ok();
    }

    #[tokio::test]
    async fn clear_cache_dir_removes_only_bin_files() {
        let cache_root = tempdir();
        std::fs::write(cache_root.join("aa.bin"), b"x").unwrap();
        std::fs::write(cache_root.join("bb.bin"), b"y").unwrap();
        std::fs::write(cache_root.join("keep.txt"), b"z").unwrap();
        let removed = clear_cache_dir(&cache_root).await.unwrap();
        assert_eq!(removed, 2);
        assert!(!cache_root.join("aa.bin").exists());
        assert!(!cache_root.join("bb.bin").exists());
        assert!(cache_root.join("keep.txt").exists());
        std::fs::remove_dir_all(&cache_root).ok();
    }
}
