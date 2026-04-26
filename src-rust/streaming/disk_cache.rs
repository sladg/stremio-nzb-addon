//! Per-session sparse cache file. Tracks populated ranges; supports random
//! reads/writes. Phase 6 layers LRU eviction over this.
//!
//! Cache layout: `{root}/{token}.bin` per active session, sized to the
//! session's `total_size` via `set_len()`. We never read holes — all reads
//! are guarded by `PopulatedRanges::contains_range`.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
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
        CachedFile::open_or_create(path, total_size).await.map(Arc::new)
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
}
