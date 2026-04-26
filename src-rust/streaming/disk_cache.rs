//! Per-session sparse cache file. Tracks populated ranges; supports random
//! reads/writes. Phase 6 layers LRU eviction over this.
//!
//! Cache layout: `{root}/{token}.bin` per active session, sized to the
//! session's `total_size` via `set_len()`. We never read holes — all reads
//! are guarded by `PopulatedRanges::contains_range`.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::streaming::ranges::PopulatedRanges;

/// Process-wide cache root + size budget. Phase 6 enforces the budget.
#[derive(Debug)]
pub struct DiskCache {
    pub root: PathBuf,
    pub max_bytes: u64,
}

impl DiskCache {
    pub fn new(root: PathBuf, max_bytes: u64) -> Self {
        Self { root, max_bytes }
    }

    /// Open (or create) the cache file for a token.
    pub async fn open(&self, token: &str, total_size: u64) -> Result<Arc<CachedFile>> {
        let path = self.root.join(format!("{token}.bin"));
        CachedFile::open_or_create(path, total_size)
            .await
            .map(Arc::new)
    }
}

/// One sparse cache file for an active session.
pub struct CachedFile {
    path: PathBuf,
    total_size: u64,
    /// `tokio::sync::Mutex` because the file ops inside the critical section
    /// are async (`seek`, `read_exact`, `write_all`).
    file: Mutex<File>,
    /// `std::sync::Mutex` because populated-range bookkeeping is pure CPU
    /// (BTreeMap insert / lookup) — the section is held for microseconds.
    /// Avoids the unnecessary task-yield overhead of `tokio::sync::Mutex`
    /// for what's effectively an in-memory data structure.
    populated: StdMutex<PopulatedRanges>,
    /// Highest assembled-byte offset already evicted via `evict_range`.
    /// `maybe_evict_behind` uses this as the lower bound of the next
    /// eviction window, so the same range never gets punched twice.
    /// Read/written hot-path so kept lock-free.
    last_evicted_to: AtomicU64,
}

impl std::fmt::Debug for CachedFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedFile")
            .field("path", &self.path)
            .field("total_size", &self.total_size)
            .finish_non_exhaustive()
    }
}

impl CachedFile {
    /// Open existing or create new sparse file at `path`, sized to `total_size`.
    /// On open of existing file we conservatively reset populated ranges to
    /// empty — we don't persist a metadata sidecar in Phase 3, so any existing
    /// content is treated as cold. Phase 6 may add a sidecar.
    pub async fn open_or_create(path: PathBuf, total_size: u64) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .await
            .with_context(|| format!("opening cache file {}", path.display()))?;
        file.set_len(total_size)
            .await
            .with_context(|| format!("sizing cache file {}", path.display()))?;
        Ok(Self {
            path,
            total_size,
            file: Mutex::new(file),
            populated: StdMutex::new(PopulatedRanges::new()),
            last_evicted_to: AtomicU64::new(0),
        })
    }

    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// True iff `[start, end)` is fully populated.
    pub fn has_range(&self, start: u64, end: u64) -> bool {
        self.populated
            .lock()
            .expect("populated mutex poisoned")
            .contains_range(start, end)
    }

    /// First gap within `[start, end)`, or None if fully populated.
    pub fn first_gap(&self, start: u64, end: u64) -> Option<(u64, u64)> {
        self.populated
            .lock()
            .expect("populated mutex poisoned")
            .first_gap(start, end)
    }

    /// Write `bytes` at `offset`. Updates populated tracking on success.
    pub async fn write_at(&self, offset: u64, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| anyhow!("write offset+len overflow"))?;
        if end > self.total_size {
            return Err(anyhow!(
                "write past end of cache file: offset {offset} len {} total {}",
                bytes.len(),
                self.total_size
            ));
        }

        let mut file = self.file.lock().await;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .context("cache seek for write")?;
        file.write_all(bytes).await.context("cache write_all")?;
        file.flush().await.context("cache flush")?;
        drop(file);

        self.populated
            .lock()
            .expect("populated mutex poisoned")
            .insert(offset, end);
        Ok(())
    }

    /// Read `len` bytes starting at `offset`. Caller must guarantee the range
    /// is populated (checked at the cache layer above for correctness, not here).
    pub async fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let end = offset
            .checked_add(len as u64)
            .ok_or_else(|| anyhow!("read offset+len overflow"))?;
        if end > self.total_size {
            return Err(anyhow!(
                "read past end of cache file: offset {offset} len {len} total {}",
                self.total_size
            ));
        }

        let mut file = self.file.lock().await;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .context("cache seek for read")?;
        let mut buf = vec![0u8; len];
        file.read_exact(&mut buf)
            .await
            .context("cache read_exact")?;
        Ok(buf)
    }

    /// Bytes currently populated (sum of intervals).
    pub fn populated_bytes(&self) -> u64 {
        self.populated
            .lock()
            .expect("populated mutex poisoned")
            .total_bytes()
    }

    /// Punch a hole in `[start, end)`, freeing real disk space (Linux), and
    /// remove the range from populated-bookkeeping so subsequent reads of
    /// these bytes correctly trigger a re-fetch via the streaming pipeline.
    ///
    /// **Platform behavior:** uses `fallocate(PUNCH_HOLE | KEEP_SIZE)` on
    /// Linux. On non-Linux (macOS dev) the syscall is a no-op — populated
    /// bookkeeping is still cleared so the *logical* eviction proceeds, but
    /// real disk usage will not shrink. Production runs on Linux.
    ///
    /// Returns the number of populated-bookkeeping bytes that were removed.
    /// Note this may be less than `end - start` if the range was already
    /// partly unpopulated; the syscall is still issued for the full range.
    pub async fn evict_range(&self, start: u64, end: u64) -> Result<u64> {
        if start >= end {
            return Ok(0);
        }
        if end > self.total_size {
            return Err(anyhow!(
                "evict past end of cache file: end {end} > total {}",
                self.total_size
            ));
        }
        // Punch the hole on the OS first; if that fails we don't want to
        // corrupt the populated tracker (which would silently zero-fill
        // those bytes on a subsequent read).
        punch_hole(&self.file, start, end - start).await?;

        // Now safe to update bookkeeping.
        let removed = self
            .populated
            .lock()
            .expect("populated mutex poisoned")
            .remove_range(start, end);

        Ok(removed)
    }

    /// Sliding-window eviction policy. Punches `[last_evicted_to,
    /// max(header_pin, playhead - backbuffer))` if that window is at least
    /// `step` bytes wide; otherwise no-op. Idempotent: subsequent calls
    /// with the same playhead do nothing.
    ///
    /// Bytes below `header_pin` are never evicted — protects video-container
    /// headers that demuxers re-read on seek.
    pub async fn maybe_evict_behind(
        &self,
        playhead: u64,
        header_pin: u64,
        backbuffer: u64,
        step: u64,
    ) -> Result<u64> {
        // The frontier we want to keep populated below the playhead.
        let keep_floor = playhead.saturating_sub(backbuffer);
        let evict_to = keep_floor.max(header_pin);
        // Floor the from-side at header_pin too — on first call the
        // watermark is 0 but we must never punch below the pin.
        let evict_from = self.last_evicted_to.load(Ordering::Relaxed).max(header_pin);

        if evict_to <= evict_from {
            return Ok(0);
        }
        if evict_to - evict_from < step {
            return Ok(0); // not worth a syscall yet
        }

        let removed = self.evict_range(evict_from, evict_to).await?;
        // Advance the watermark so we don't re-punch the same range. This is
        // a single-writer pattern (only the foreground yield loop calls it
        // for a given session) so a simple store is fine — no CAS needed.
        self.last_evicted_to.store(evict_to, Ordering::Relaxed);
        tracing::debug!(
            "[cache] evicted {removed} bytes [{evict_from}..{evict_to}) playhead={playhead} pin={header_pin} backbuf={backbuffer}",
        );
        Ok(removed)
    }

    /// Eviction watermark — for tests/observability.
    #[cfg(test)]
    pub fn last_evicted_to(&self) -> u64 {
        self.last_evicted_to.load(Ordering::Relaxed)
    }
}

/// Linux: `fallocate(PUNCH_HOLE | KEEP_SIZE, offset, len)`. Non-Linux: no-op.
#[cfg(target_os = "linux")]
async fn punch_hole(file: &Mutex<File>, offset: u64, len: u64) -> Result<()> {
    use rustix::fs::{fallocate, FallocateFlags};
    use std::os::fd::AsFd;
    let guard = file.lock().await;
    let fd = guard.as_fd();
    fallocate(
        fd,
        FallocateFlags::PUNCH_HOLE | FallocateFlags::KEEP_SIZE,
        offset,
        len,
    )
    .with_context(|| format!("fallocate PUNCH_HOLE offset={offset} len={len}"))?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
async fn punch_hole(_file: &Mutex<File>, _offset: u64, _len: u64) -> Result<()> {
    // No-op on macOS dev — populated bookkeeping is still cleared by the
    // caller so the logical eviction proceeds; only real disk usage
    // (visible via `du`/`stat` blocks) is unaffected.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nzb-cache-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn write_then_read_round_trip() {
        let dir = tempdir();
        let cache = DiskCache::new(dir.clone(), 1024 * 1024);
        let f = cache.open("tok1", 1000).await.unwrap();
        f.write_at(100, b"hello world").await.unwrap();
        assert!(f.has_range(100, 111));
        let bytes = f.read_at(100, 11).await.unwrap();
        assert_eq!(&bytes, b"hello world");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn has_range_matches_populated_intervals() {
        let dir = tempdir();
        let cache = DiskCache::new(dir.clone(), 1024 * 1024);
        let f = cache.open("tok2", 1000).await.unwrap();
        f.write_at(0, b"AAAA").await.unwrap();
        f.write_at(10, b"BBBB").await.unwrap();
        assert!(f.has_range(0, 4));
        assert!(f.has_range(10, 14));
        assert!(!f.has_range(4, 10)); // gap
        assert!(!f.has_range(0, 14)); // not contiguous
        f.write_at(4, b"GAP_FILL!!").await.unwrap();
        assert!(f.has_range(0, 14));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn first_gap_walks_holes() {
        let dir = tempdir();
        let cache = DiskCache::new(dir.clone(), 1024 * 1024);
        let f = cache.open("tok3", 1000).await.unwrap();
        f.write_at(0, b"AAAA").await.unwrap(); // [0..4)
        f.write_at(20, b"BBBB").await.unwrap(); // [20..24)
        assert_eq!(f.first_gap(0, 30), Some((4, 20)));
        assert_eq!(f.first_gap(0, 4), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn write_past_end_errors() {
        let dir = tempdir();
        let cache = DiskCache::new(dir.clone(), 1024 * 1024);
        let f = cache.open("tok4", 100).await.unwrap();
        let res = f.write_at(95, b"too long for the buffer").await;
        assert!(res.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn populated_bytes_tracks_writes() {
        let dir = tempdir();
        let cache = DiskCache::new(dir.clone(), 1024 * 1024);
        let f = cache.open("tok5", 1000).await.unwrap();
        f.write_at(0, b"abcd").await.unwrap();
        f.write_at(100, b"xyz").await.unwrap();
        assert_eq!(f.populated_bytes(), 7);
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---------- eviction ----------

    #[tokio::test]
    async fn evict_range_clears_populated_bookkeeping() {
        let dir = tempdir();
        let cache = DiskCache::new(dir.clone(), 1024 * 1024);
        let f = cache.open("evict1", 4096).await.unwrap();
        // Populate two regions.
        f.write_at(0, &vec![1u8; 1024]).await.unwrap();
        f.write_at(2048, &vec![2u8; 1024]).await.unwrap();
        assert_eq!(f.populated_bytes(), 2048);

        // Evict the first region.
        let removed = f.evict_range(0, 1024).await.unwrap();
        assert_eq!(removed, 1024);
        assert!(!f.has_range(0, 1024), "evicted range must not be populated");
        assert!(
            f.has_range(2048, 3072),
            "untouched range must still be populated"
        );
        assert_eq!(f.populated_bytes(), 1024);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn evict_range_partial_overlap_is_correct() {
        let dir = tempdir();
        let cache = DiskCache::new(dir.clone(), 1024 * 1024);
        let f = cache.open("evict2", 1000).await.unwrap();
        f.write_at(100, &vec![0xAAu8; 200]).await.unwrap(); // [100..300)
                                                            // Evict bytes [200..400) — should trim the right side of the
                                                            // populated interval.
        let removed = f.evict_range(200, 400).await.unwrap();
        assert_eq!(
            removed, 100,
            "only [200..300) was populated of the evicted range"
        );
        assert!(f.has_range(100, 200), "[100..200) should remain populated");
        assert!(!f.has_range(200, 300));
        assert_eq!(f.populated_bytes(), 100);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn evict_past_end_errors() {
        let dir = tempdir();
        let cache = DiskCache::new(dir.clone(), 1024 * 1024);
        let f = cache.open("evict3", 100).await.unwrap();
        assert!(f.evict_range(50, 200).await.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn maybe_evict_behind_respects_header_pin() {
        // Fully populate a 10 MiB sparse file. Playhead at 8 MiB,
        // header_pin = 1 MiB, backbuffer = 0, step = 1 MiB. Eviction should
        // punch [0..1 MiB)?? Actually evict_to = max(header_pin=1MiB,
        // playhead - backbuffer = 8MiB) = 8MiB; from = last_evicted_to = 0;
        // 8MiB - 0 = 8MiB ≥ step → punches [0, 8MiB). But header_pin=1MiB
        // means we shouldn't touch [0..1MiB). The current policy starts
        // eviction at last_evicted_to (0), so it WOULD include the header
        // region. Verify the policy clamps the floor to header_pin.
        let dir = tempdir();
        let cache = DiskCache::new(dir.clone(), 50 * 1024 * 1024);
        let total = 10 * 1024 * 1024u64;
        let f = cache.open("evict4", total).await.unwrap();
        f.write_at(0, &vec![0u8; total as usize]).await.unwrap();
        assert_eq!(f.populated_bytes(), total);

        // Trigger eviction with a 1 MiB header pin and zero backbuffer.
        let header_pin = 1024 * 1024u64;
        let evicted = f
            .maybe_evict_behind(8 * 1024 * 1024, header_pin, 0, 1024 * 1024)
            .await
            .unwrap();
        assert!(evicted > 0, "should have evicted something");

        // Header region must still be populated.
        assert!(
            f.has_range(0, header_pin),
            "header pin region must survive eviction"
        );
        // Region just past the pin should be evicted.
        assert!(
            !f.has_range(header_pin, header_pin + 1024),
            "region above header pin should be evicted"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn maybe_evict_behind_skips_below_step_threshold() {
        let dir = tempdir();
        let cache = DiskCache::new(dir.clone(), 50 * 1024 * 1024);
        let f = cache.open("evict5", 10 * 1024 * 1024).await.unwrap();
        f.write_at(0, &vec![0u8; 10 * 1024 * 1024]).await.unwrap();

        // Playhead = 1 MiB, backbuffer = 0, step = 4 MiB. Eviction window
        // would be [0..1 MiB) = 1 MiB, below the 4 MiB step threshold.
        let evicted = f
            .maybe_evict_behind(1024 * 1024, 0, 0, 4 * 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(evicted, 0, "should not evict below step threshold");
        assert_eq!(f.last_evicted_to(), 0, "watermark must not advance");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn maybe_evict_behind_advances_watermark_idempotently() {
        let dir = tempdir();
        let cache = DiskCache::new(dir.clone(), 50 * 1024 * 1024);
        let total = 20 * 1024 * 1024u64;
        let f = cache.open("evict6", total).await.unwrap();
        f.write_at(0, &vec![0u8; total as usize]).await.unwrap();

        // First call: playhead at 10 MiB, no backbuffer, step = 1 MiB.
        // Should evict [0..10 MiB) and advance watermark to 10 MiB.
        let first = f
            .maybe_evict_behind(10 * 1024 * 1024, 0, 0, 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(first, 10 * 1024 * 1024);
        assert_eq!(f.last_evicted_to(), 10 * 1024 * 1024);

        // Second call: same playhead. Should be a no-op (idempotent).
        let second = f
            .maybe_evict_behind(10 * 1024 * 1024, 0, 0, 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(
            second, 0,
            "second call with same playhead must not re-punch"
        );
        assert_eq!(f.last_evicted_to(), 10 * 1024 * 1024);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn evicted_range_can_be_re_populated() {
        // Sanity: after eviction, the same range can be written again
        // (i.e. punching holes hasn't broken the file's writability).
        let dir = tempdir();
        let cache = DiskCache::new(dir.clone(), 1024 * 1024);
        let f = cache.open("evict7", 4096).await.unwrap();
        f.write_at(0, &vec![0xCDu8; 1024]).await.unwrap();
        f.evict_range(0, 1024).await.unwrap();
        assert!(!f.has_range(0, 1024));
        f.write_at(0, &vec![0xEFu8; 1024]).await.unwrap();
        assert!(f.has_range(0, 1024));
        let bytes = f.read_at(0, 1024).await.unwrap();
        assert!(
            bytes.iter().all(|&b| b == 0xEF),
            "re-written bytes must read back"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
