//! edge-cdn-store
//!
//! A CDN cache storage backend for `pingora_cache` focused on:
//! - high concurrency via DashMap for metadata and in-progress writes
//! - async disk persistence (Tokio), with io_uring planned
//! - streaming partial writes with reader attachment by tag
//! - Prometheus metrics, and compatibility with Pingora's eviction manager
//! - simple YAML-based configuration (using Pingora's YAML with namespaced keys)
//!
//! Configuration keys (top-level, parsed by this crate):
//! - `edge-cdn-cache-disk-root`: string path for on-disk data (env fallback: `EDGE_STORE_DIR`)
//! - `edge-cdn-cache-max-disk-bytes`: optional hard cap on approximate bytes
//! - `edge-cdn-cache-max-object-bytes`: optional hard cap per object
//! - `edge-cdn-cache-max-partial-writes`: optional cap on concurrent in-progress writers
//! - `edge-cdn-cache-atomic-publish`: planned
//! - `edge-cdn-cache-io-uring-enabled`: planned
//!
//! Metrics exported:
//! - counters: `edge_cache_hits_total`, `edge_cache_misses_total`,
//!   `edge_cache_evictions_total`, `edge_cache_invalidations_total`,
//!   `edge_cache_stream_attach_total`, `edge_cache_stream_attach_miss_total`
//! - gauges: `edge_cache_keys`, `edge_cache_partial_writes`, `edge_cache_disk_bytes`
//!
//! See README.md and design/ for deeper docs and examples.
use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashMap;
pub mod config;
pub use config::{EdgeMemoryStorageBuilder, EdgeStoreConfig};
use once_cell::sync::Lazy;
use pingora::Error;
use pingora::Result;
use pingora::cache::key::{CacheHashKey, CompactCacheKey};
use pingora::cache::storage::{
    HandleHit, HandleMiss, HitHandler, MissFinishType, MissHandler, PurgeType, Storage,
};
use pingora::cache::trace::SpanHandle;
use pingora::cache::{CacheKey, CacheMeta};
use pingora::{ErrorType, OrErr};
use prometheus::{IntCounter, IntGauge, register_int_counter, register_int_gauge};
use prometheus::{Histogram, register_histogram};
use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{RwLock, watch};

// Prometheus metrics
static MET_HITS: Lazy<IntCounter> =
    Lazy::new(|| register_int_counter!("edge_cache_hits_total", "Total cache hits").unwrap());
static MET_MISSES: Lazy<IntCounter> =
    Lazy::new(|| register_int_counter!("edge_cache_misses_total", "Total cache misses").unwrap());
static MET_PURGE_EVICTIONS: Lazy<IntCounter> =
    Lazy::new(|| register_int_counter!("edge_cache_evictions_total", "Total evictions").unwrap());
static MET_PURGE_INVALIDATIONS: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("edge_cache_invalidations_total", "Total invalidations").unwrap()
});
static GAUGE_KEYS: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!("edge_cache_keys", "Number of complete cached objects").unwrap()
});
static GAUGE_PARTIALS: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!("edge_cache_partial_writes", "Number of in-progress writes").unwrap()
});
static GAUGE_DISK_BYTES: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!(
        "edge_cache_disk_bytes",
        "Approximate total bytes of cached bodies on disk"
    )
    .unwrap()
});
static MET_STREAM_ATTACH_OK: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "edge_cache_stream_attach_total",
        "Successful streaming write tag attachments"
    )
    .unwrap()
});
static MET_STREAM_ATTACH_MISS: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "edge_cache_stream_attach_miss_total",
        "Failed streaming write tag attachments"
    )
    .unwrap()
});
static MET_DEMOTED_TO_DISK: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "edge_cache_demotions_total",
        "Number of entries demoted from memory to disk"
    )
    .unwrap()
});
static HIST_DISK_WRITE_SECS: Lazy<Histogram> = Lazy::new(|| {
    register_histogram!(
        "edge_cache_disk_write_seconds",
        "Latency of disk persists in seconds"
    )
    .unwrap()
});
static HIST_DISK_READ_SECS: Lazy<Histogram> = Lazy::new(|| {
    register_histogram!(
        "edge_cache_disk_read_seconds",
        "Latency of disk read ops in seconds"
    )
    .unwrap()
});

/// DashMap-based in-memory storage for pingora cache
pub struct EdgeMemoryStorage {
    /// Complete cached objects ready to serve
    cache: Arc<DashMap<String, CacheObject>>,

    /// Partial writes in progress - supports concurrent streaming writes
    partial: Arc<DashMap<String, DashMap<u64, PartialObject>>>,

    /// Atomic counter for unique write IDs
    pub write_counter: AtomicU64,

    /// Root directory for on-disk persistence
    pub disk_root: PathBuf,

    // Capacity controls (optional)
    pub max_object_bytes: Option<usize>,
    pub max_partial_writes: Option<usize>,
    pub max_disk_bytes: Option<usize>,
    // Accounting
    pub partial_count: AtomicUsize,
    pub disk_bytes_used: AtomicUsize,
    /// Use atomic publish for on-disk persistence
    pub atomic_publish: bool,
}

/// A complete cached object ready for immediate serving
struct CacheObject {
    /// Serialized cache metadata (internal, header)
    meta: (Vec<u8>, Vec<u8>),
    /// Complete response body
    body: Arc<Vec<u8>>,
}

/// A partial cached object currently being written
struct PartialObject {
    /// Serialized cache metadata (internal, header)
    meta: (Vec<u8>, Vec<u8>),
    /// Growing response body (protected by RwLock for concurrent access)
    body: Arc<RwLock<Vec<u8>>>,
    /// Notifies readers when new bytes are available
    notify: Arc<watch::Sender<usize>>,
    /// Streaming write identifier as bytes for matching
    write_id_bytes: [u8; 8],
}

impl EdgeMemoryStorage {
    /// Provide a default instance using `new()`.
    pub fn default_instance() -> Self {
        Self::new()
    }
    /// Create a new EdgeMemoryStorage instance
    pub fn new() -> Self {
        // Default disk root can be overridden by env var
        let root =
            std::env::var("EDGE_STORE_DIR").unwrap_or_else(|_| "edge_store_data".to_string());
        Self::with_disk_root(PathBuf::from(root))
    }

    /// Create with explicit disk root directory
    pub fn with_disk_root(disk_root: PathBuf) -> Self {
        Self {
            cache: Arc::new(DashMap::new()),
            partial: Arc::new(DashMap::new()),
            write_counter: AtomicU64::new(0),
            disk_root,
            max_object_bytes: None,
            max_partial_writes: None,
            max_disk_bytes: None,
            partial_count: AtomicUsize::new(0),
            disk_bytes_used: AtomicUsize::new(0),
            atomic_publish: false,
        }
    }

    /// Create from an EdgeStoreConfig (parsed from YAML) via the builder.
    pub fn from_config(cfg: &EdgeStoreConfig) -> Self {
        EdgeMemoryStorageBuilder::new()
            .with_disk_root(cfg.resolve_disk_root())
            .with_max_disk_bytes(cfg.max_disk_bytes)
            .with_max_object_bytes(cfg.max_object_bytes)
            .with_max_partial_writes(cfg.max_partial_writes)
            .with_atomic_publish(cfg.atomic_publish)
            .with_io_uring_enabled(cfg.io_uring_enabled)
            .build()
    }

    /// Start a builder for advanced configuration
    pub fn builder() -> EdgeMemoryStorageBuilder {
        EdgeMemoryStorageBuilder::new()
    }

    /// Generate unique write ID for streaming writes
    fn next_write_id(&self) -> u64 {
        self.write_counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Convert cache key to string for DashMap indexing
    fn key_to_hash(&self, key: &CacheKey) -> String {
        key.combined()
    }

    /// Convert compact cache key to string for DashMap indexing
    fn compact_key_to_hash(&self, key: &CompactCacheKey) -> String {
        key.combined()
    }

    /// Hash string for filesystem-safe path
    fn hash_str(s: &str) -> String {
        // Use a simple, fast hash for pathing; blake3 not required for MVP
        // FNV-like 64-bit hash, hex-encoded
        let mut hash: u64 = 0xcbf29ce484222325;
        for b in s.as_bytes() {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        format!("{hash:016x}")
    }

    /// Compute directory path for a key hash
    fn key_dir(&self, key_hash: &str) -> PathBuf {
        let (a, b) = key_hash.split_at(2);
        self.disk_root.join(a).join(&b[0..2]).join(key_hash)
    }

    /// Paths for body and meta
    fn body_path(dir: &Path) -> PathBuf {
        dir.join("body.bin")
    }
    fn meta_header_path(dir: &Path) -> PathBuf {
        dir.join("meta_header.bin")
    }
    fn meta_internal_path(dir: &Path) -> PathBuf {
        dir.join("meta_internal.bin")
    }
}

impl Default for EdgeMemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

/// Handler for reading complete cached objects
pub struct CompleteHitHandler {
    body: Arc<Vec<u8>>,
    position: usize,
    range_start: usize,
    range_end: usize,
}

impl CompleteHitHandler {
    fn new(body: Arc<Vec<u8>>) -> Self {
        let len = body.len();
        Self {
            body,
            position: 0,
            range_start: 0,
            range_end: len,
        }
    }

    fn read_chunk(&mut self) -> Option<Bytes> {
        if self.position >= self.range_end {
            return None;
        }

        let start = self.position;
        let end = self.range_end;
        self.position = end; // Read all remaining data at once

        Some(Bytes::copy_from_slice(&self.body[start..end]))
    }
}

#[async_trait]
impl HandleHit for CompleteHitHandler {
    async fn read_body(&mut self) -> Result<Option<Bytes>> {
        Ok(self.read_chunk())
    }

    async fn finish(
        self: Box<Self>,
        _storage: &'static (dyn Storage + Sync),
        _key: &CacheKey,
        _trace: &SpanHandle,
    ) -> Result<()> {
        // Nothing to cleanup for complete hits
        Ok(())
    }

    fn can_seek(&self) -> bool {
        true
    }

    fn seek(&mut self, start: usize, end: Option<usize>) -> Result<()> {
        if start >= self.body.len() {
            return Err(pingora::Error::explain(
                pingora::ErrorType::InternalError,
                format!(
                    "Seek start {} exceeds body length {}",
                    start,
                    self.body.len()
                ),
            ));
        }

        self.range_start = start;
        self.range_end = end.unwrap_or(self.body.len()).min(self.body.len());
        self.position = self.range_start;

        Ok(())
    }

    fn get_eviction_weight(&self) -> usize {
        self.body.len()
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }

    fn as_any_mut(&mut self) -> &mut (dyn Any + Send + Sync) {
        self
    }
}

/// Handler for reading partial cached objects (streaming writes)
pub struct PartialHitHandler {
    body: Arc<RwLock<Vec<u8>>>,
    notify: watch::Receiver<usize>,
    position: usize,
}

impl PartialHitHandler {
    fn new(body: Arc<RwLock<Vec<u8>>>, notify: watch::Receiver<usize>) -> Self {
        Self {
            body,
            notify,
            position: 0,
        }
    }

    async fn read_available(&mut self) -> Option<Bytes> {
        loop {
            let available_bytes = *self.notify.borrow_and_update();

            // Handle EOF marker first to avoid slicing with usize::MAX
            if available_bytes == usize::MAX {
                let body = self.body.read().await;
                if self.position < body.len() {
                    let chunk = Bytes::copy_from_slice(&body[self.position..]);
                    self.position = body.len();
                    return Some(chunk);
                }
                return None;
            }

            if available_bytes > self.position {
                // New data is available
                let body = self.body.read().await;
                let end = available_bytes.min(body.len());
                if self.position < end {
                    let chunk = Bytes::copy_from_slice(&body[self.position..end]);
                    self.position = end;
                    return Some(chunk);
                }
            }

            // Wait for more data
            if self.notify.changed().await.is_err() {
                // Writer finished - check for any remaining data
                let body = self.body.read().await;
                if self.position < body.len() {
                    let chunk = Bytes::copy_from_slice(&body[self.position..]);
                    self.position = body.len();
                    return Some(chunk);
                }
                return None;
            }
        }
    }
}

#[async_trait]
impl HandleHit for PartialHitHandler {
    async fn read_body(&mut self) -> Result<Option<Bytes>> {
        Ok(self.read_available().await)
    }

    async fn finish(
        self: Box<Self>,
        _storage: &'static (dyn Storage + Sync),
        _key: &CacheKey,
        _trace: &SpanHandle,
    ) -> Result<()> {
        // Nothing special to cleanup for partial hits
        Ok(())
    }

    fn can_seek(&self) -> bool {
        false // Seeking not supported for partial reads
    }

    fn seek(&mut self, _start: usize, _end: Option<usize>) -> Result<()> {
        Err(pingora::Error::explain(
            pingora::ErrorType::InternalError,
            "Seeking not supported for partial cached objects",
        ))
    }

    fn should_count_access(&self) -> bool {
        false // Don't count partial reads to avoid skewing eviction
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }

    fn as_any_mut(&mut self) -> &mut (dyn Any + Send + Sync) {
        self
    }
}

/// Handler for writing new cache entries
pub struct EdgeMissHandler {
    key: String,
    write_id: u64,
    write_id_bytes: [u8; 8],
    body: Arc<RwLock<Vec<u8>>>,
    notify: Arc<watch::Sender<usize>>,
    cache: Arc<DashMap<String, CacheObject>>,
    partial: Arc<DashMap<String, DashMap<u64, PartialObject>>>,
    meta: (Vec<u8>, Vec<u8>),
    /// Disk location info
    disk_dir: PathBuf,
    /// Limits
    max_object_bytes: Option<usize>,
    max_disk_bytes: Option<usize>,
    partial_count: &'static AtomicUsize,
    disk_bytes_used: &'static AtomicUsize,
    /// Internal: ensure cleanup (map removal + counters) happens exactly once
    cleaned: bool,
    /// Whether to use atomic publish for disk persistence
    atomic_publish: bool,
}

#[async_trait]
impl HandleMiss for EdgeMissHandler {
    async fn write_body(&mut self, data: Bytes, eof: bool) -> Result<()> {
        let mut body = self.body.write().await;
        // Enforce max_object_bytes if configured
        if let Some(limit) = self.max_object_bytes {
            if body.len().saturating_add(data.len()) > limit {
                return Error::e_explain(
                    ErrorType::Custom("ObjectTooLarge"),
                    format!("object exceeds max_object_bytes={limit}"),
                );
            }
        }
        body.extend_from_slice(&data);
        let new_len = body.len();
        drop(body); // Release lock before sending notification

        if eof {
            // Signal EOF with special marker
            let _ = self.notify.send(usize::MAX);
        } else {
            // Notify readers of new available bytes
            let _ = self.notify.send(new_len);
        }

        Ok(())
    }

    async fn finish(self: Box<Self>) -> Result<MissFinishType> {
        // Move from partial to complete cache
        let body = self.body.read().await;
        let body_size = body.len();
        let complete_body = body.clone();
        drop(body); // Release read lock

        // Enforce disk capacity if configured
        if let Some(limit) = self.max_object_bytes {
            // already enforced during write, but keep as safety net
            if complete_body.len() > limit {
                // cleanup partial entry
                if let Some(partial_map) = self.partial.get(&self.key) {
                    let removed = partial_map.remove(&self.write_id).is_some();
                    let empty = partial_map.is_empty();
                    drop(partial_map);
                    if empty {
                        self.partial.remove(&self.key);
                    }
                    if removed {
                        GAUGE_PARTIALS.dec();
                    }
                }
                self.partial_count.fetch_sub(1, Ordering::Relaxed);
                return Error::e_explain(
                    ErrorType::Custom("ObjectTooLarge"),
                    format!("object exceeds max_object_bytes={limit} on finish"),
                );
            }
        }
        // Enforce disk bytes budget if configured (approximate)
        if let Some(limit) = self.max_disk_bytes {
            let used = self.disk_bytes_used.load(Ordering::Relaxed);
            let needed = used.saturating_add(complete_body.len());
            if needed > limit {
                // cleanup partial entry
                if let Some(partial_map) = self.partial.get(&self.key) {
                    let removed = partial_map.remove(&self.write_id).is_some();
                    let empty = partial_map.is_empty();
                    drop(partial_map);
                    if empty {
                        self.partial.remove(&self.key);
                    }
                    if removed {
                        GAUGE_PARTIALS.dec();
                    }
                }
                self.partial_count.fetch_sub(1, Ordering::Relaxed);
                return Error::e_explain(
                    ErrorType::Custom("DiskCapacityExceeded"),
                    format!(
                        "disk usage {used} + {} exceeds max_disk_bytes={limit}",
                        complete_body.len()
                    ),
                );
            }
        }

        // Approximate disk usage accounting: add body size (ignore meta)
        if self.max_disk_bytes.is_some() {
            let after = self.disk_bytes_used.fetch_add(body_size, Ordering::Relaxed) + body_size;
            GAUGE_DISK_BYTES.set(after as i64);
        }

        // Insert into complete in-memory cache immediately for fast hits
        let body_arc = Arc::new(complete_body);
        let cache_object = CacheObject {
            meta: self.meta.clone(),
            body: body_arc.clone(),
        };
        self.cache.insert(self.key.clone(), cache_object);
        GAUGE_KEYS.set(self.cache.len() as i64);

        // Persist to disk in background; after success, drop in-memory body to rely on disk
        let dir = self.disk_dir.clone();
        let (internal, header) = self.meta.clone();
        let key_to_remove = self.key.clone();
        let cache_for_remove = Arc::clone(&self.cache);
        let use_atomic = self.atomic_publish;
        tokio::spawn(async move {
            let start = std::time::Instant::now();
            let res = if use_atomic {
                persist_to_disk_atomic(dir, &internal, &header, &body_arc).await
            } else {
                persist_to_disk(dir, &internal, &header, &body_arc).await
            };
            if res.is_ok() {
                HIST_DISK_WRITE_SECS.observe(start.elapsed().as_secs_f64());
                if cache_for_remove.remove(&key_to_remove).is_some() {
                    GAUGE_KEYS.dec();
                    MET_DEMOTED_TO_DISK.inc();
                }
            }
        });

        // Ensure cleanup (map removal + counters) only happens once
        let mut this = self;
        this.cleanup_once();

        Ok(MissFinishType::Created(body_size))
    }

    fn streaming_write_tag(&self) -> Option<&[u8]> {
        Some(&self.write_id_bytes)
    }
}

impl Drop for EdgeMissHandler {
    fn drop(&mut self) {
        // Clean up partial cache entry if handler is dropped without finishing
        self.cleanup_once();
    }
}

impl EdgeMissHandler {
    fn cleanup_once(&mut self) {
        if std::mem::take(&mut self.cleaned) {
            // already cleaned
            return;
        }
        if let Some(partial_map) = self.partial.get(&self.key) {
            let removed = partial_map.remove(&self.write_id).is_some();
            let empty = partial_map.is_empty();
            drop(partial_map);
            if empty {
                self.partial.remove(&self.key);
            }
            if removed {
                GAUGE_PARTIALS.dec();
            }
        }
        self.partial_count.fetch_sub(1, Ordering::Relaxed);
        // mark as cleaned
        self.cleaned = true;
    }
}

// Persist meta and body to disk under dir
async fn persist_to_disk(
    dir: PathBuf,
    meta_internal: &[u8],
    meta_header: &[u8],
    body: &Arc<Vec<u8>>,
) -> Result<()> {
    fs::create_dir_all(&dir)
        .await
        .or_err(ErrorType::FileCreateError, "create cache dir")?;
    let mut f = fs::File::create(EdgeMemoryStorage::body_path(&dir))
        .await
        .or_err(ErrorType::FileCreateError, "create body file")?;
    f.write_all(body)
        .await
        .or_err(ErrorType::FileWriteError, "write body file")?;
    f.flush()
        .await
        .or_err(ErrorType::FileWriteError, "flush body file")?;
    drop(f);
    let mut mi = fs::File::create(EdgeMemoryStorage::meta_internal_path(&dir))
        .await
        .or_err(ErrorType::FileCreateError, "create internal meta file")?;
    mi.write_all(meta_internal)
        .await
        .or_err(ErrorType::FileWriteError, "write internal meta file")?;
    mi.flush()
        .await
        .or_err(ErrorType::FileWriteError, "flush internal meta file")?;
    drop(mi);
    let mut mh = fs::File::create(EdgeMemoryStorage::meta_header_path(&dir))
        .await
        .or_err(ErrorType::FileCreateError, "create header meta file")?;
    mh.write_all(meta_header)
        .await
        .or_err(ErrorType::FileWriteError, "write header meta file")?;
    mh.flush()
        .await
        .or_err(ErrorType::FileWriteError, "flush header meta file")?;
    drop(mh);
    Ok(())
}

// Atomic publish variant: write to a temp directory and atomically rename
async fn persist_to_disk_atomic(
    dir: PathBuf,
    meta_internal: &[u8],
    meta_header: &[u8],
    body: &Arc<Vec<u8>>,
) -> Result<()> {
    // temp path sibling (e.g., <dir>.tmp)
    let tmp = dir.with_extension("tmp");
    // Ensure parent exists
    if let Some(parent) = tmp.parent() {
        fs::create_dir_all(parent)
            .await
            .or_err(ErrorType::FileCreateError, "create parent dir")?;
    }
    // Clean previous tmp if present
    let _ = fs::remove_dir_all(&tmp).await;
    fs::create_dir_all(&tmp)
        .await
        .or_err(ErrorType::FileCreateError, "create tmp cache dir")?;
    // Write files into tmp dir and fsync them
    let mut f = fs::File::create(EdgeMemoryStorage::body_path(&tmp))
        .await
        .or_err(ErrorType::FileCreateError, "create tmp body file")?;
    f.write_all(body)
        .await
        .or_err(ErrorType::FileWriteError, "write tmp body file")?;
    f.sync_all()
        .await
        .or_err(ErrorType::FileWriteError, "sync tmp body file")?;
    drop(f);

    let mut mi = fs::File::create(EdgeMemoryStorage::meta_internal_path(&tmp))
        .await
        .or_err(ErrorType::FileCreateError, "create tmp internal meta file")?;
    mi.write_all(meta_internal)
        .await
        .or_err(ErrorType::FileWriteError, "write tmp internal meta file")?;
    mi.sync_all()
        .await
        .or_err(ErrorType::FileWriteError, "sync tmp internal meta file")?;
    drop(mi);

    let mut mh = fs::File::create(EdgeMemoryStorage::meta_header_path(&tmp))
        .await
        .or_err(ErrorType::FileCreateError, "create tmp header meta file")?;
    mh.write_all(meta_header)
        .await
        .or_err(ErrorType::FileWriteError, "write tmp header meta file")?;
    mh.sync_all()
        .await
        .or_err(ErrorType::FileWriteError, "sync tmp header meta file")?;
    drop(mh);

    // Best-effort fsync of tmp dir parent using blocking std API
    if let Some(parent) = tmp.parent() {
        let parent = parent.to_path_buf();
        let _ = tokio::task::spawn_blocking(move || {
            if let Ok(dir_file) = std::fs::File::open(&parent) {
                let _ = dir_file.sync_all();
            }
        })
        .await;
    }

    // Rename tmp -> final atomically
    // Remove existing final dir if present (best-effort)
    let _ = fs::remove_dir_all(&dir).await;
    fs::rename(&tmp, &dir)
        .await
        .or_err(ErrorType::FileWriteError, "rename tmp->final")?;
    Ok(())
}

// Check disk paths exist and load meta; open body file for disk-backed handler
async fn open_disk_hit(dir: &Path) -> Result<Option<((Vec<u8>, Vec<u8>), fs::File)>> {
    let body_path = EdgeMemoryStorage::body_path(dir);
    let meta_header_path = EdgeMemoryStorage::meta_header_path(dir);
    let meta_internal_path = EdgeMemoryStorage::meta_internal_path(dir);
    if fs::try_exists(&body_path)
        .await
        .or_err(ErrorType::InternalError, "check body exists")?
        && fs::try_exists(&meta_header_path)
            .await
            .or_err(ErrorType::InternalError, "check header meta exists")?
        && fs::try_exists(&meta_internal_path)
            .await
            .or_err(ErrorType::InternalError, "check internal meta exists")?
    {
        let mut mif = fs::File::open(meta_internal_path)
            .await
            .or_err(ErrorType::FileOpenError, "open internal meta file")?;
        let mut mi = Vec::new();
        mif.read_to_end(&mut mi)
            .await
            .or_err(ErrorType::FileReadError, "read internal meta file")?;

        let mut mhf = fs::File::open(meta_header_path)
            .await
            .or_err(ErrorType::FileOpenError, "open header meta file")?;
        let mut mh = Vec::new();
        mhf.read_to_end(&mut mh)
            .await
            .or_err(ErrorType::FileReadError, "read header meta file")?;

        let body_file = fs::File::open(body_path)
            .await
            .or_err(ErrorType::FileOpenError, "open body file")?;

        Ok(Some(((mi, mh), body_file)))
    } else {
        Ok(None)
    }
}

async fn open_disk_hit_with_retry(dir: &Path) -> Result<Option<((Vec<u8>, Vec<u8>), fs::File)>> {
    let mut attempt = 0;
    loop {
        match open_disk_hit(dir).await {
            Ok(res) => return Ok(res),
            Err(e) => {
                attempt += 1;
                if attempt >= 3 {
                    return Err(e);
                }
                tokio::time::sleep(std::time::Duration::from_millis(10 * attempt)).await;
                continue;
            }
        }
    }
}

/// Hit handler that reads directly from disk in chunks (not used in MVP hot path)
struct DiskHitHandler {
    file: fs::File,
    position: u64,
    end: Option<u64>,
}

#[async_trait]
impl HandleHit for DiskHitHandler {
    async fn read_body(&mut self) -> Result<Option<Bytes>> {
        // Respect range end if set and seek to current position
        let start = std::time::Instant::now();
        let to_read = match self.end {
            Some(end) if end <= self.position => 0,
            Some(end) => (end - self.position).min(64 * 1024) as usize,
            None => 64 * 1024,
        };
        if to_read == 0 {
            return Ok(None);
        }
        let mut buf = vec![0u8; to_read];
        use tokio::io::AsyncSeekExt;
        self
            .file
            .seek(std::io::SeekFrom::Start(self.position))
            .await
            .or_err(ErrorType::FileReadError, "disk hit seek")?;
        let n = self
            .file
            .read(&mut buf)
            .await
            .or_err(ErrorType::FileReadError, "disk hit read")?;
        HIST_DISK_READ_SECS.observe(start.elapsed().as_secs_f64());
        if n == 0 {
            return Ok(None);
        }
        buf.truncate(n);
        self.position += n as u64;
        Ok(Some(Bytes::from(buf)))
    }

    async fn finish(
        self: Box<Self>,
        _storage: &'static (dyn Storage + Sync),
        _key: &CacheKey,
        _trace: &SpanHandle,
    ) -> Result<()> {
        Ok(())
    }

    fn can_seek(&self) -> bool {
        true
    }
    fn seek(&mut self, start: usize, end: Option<usize>) -> Result<()> {
        self.position = start as u64;
        self.end = end.map(|e| e as u64);
        Ok(())
    }
    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }
    fn as_any_mut(&mut self) -> &mut (dyn Any + Send + Sync) {
        self
    }
}

#[async_trait]
impl Storage for EdgeMemoryStorage {
    async fn lookup(
        &'static self,
        key: &CacheKey,
        _trace: &SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        let key_s = self.key_to_hash(key);
        if let Some(obj) = self.cache.get(&key_s) {
            // Reconstruct CacheMeta from serialized parts
            let (internal, header) = (&obj.meta.0, &obj.meta.1);
            let meta = CacheMeta::deserialize(internal, header)?;
            let handler: HitHandler = Box::new(CompleteHitHandler::new(obj.body.clone()));
            MET_HITS.inc();
            return Ok(Some((meta, handler)));
        }

        // Check partial writes
        if let Some(partials) = self.partial.get(&key_s) {
            if let Some(entry) = partials.iter().next() {
                let po = entry.value();
                let rx = po.notify.subscribe();
                let handler: HitHandler = Box::new(PartialHitHandler::new(po.body.clone(), rx));
                let (internal, header) = (&po.meta.0, &po.meta.1);
                let meta = CacheMeta::deserialize(internal, header)?;
                MET_HITS.inc();
                return Ok(Some((meta, handler)));
            }
        }

        // Disk-backed hit without loading whole body into memory
        let hash = Self::hash_str(&key_s);
        let dir = self.key_dir(&hash);
        if let Some(((mi, mh), file)) = open_disk_hit_with_retry(&dir).await? {
            let meta = CacheMeta::deserialize(&mi, &mh)?;
            let handler: HitHandler = Box::new(DiskHitHandler {
                file,
                position: 0,
                end: None,
            });
            MET_HITS.inc();
            return Ok(Some((meta, handler)));
        }

        MET_MISSES.inc();
        Ok(None)
    }

    async fn lookup_streaming_write(
        &'static self,
        key: &CacheKey,
        streaming_write_tag: Option<&[u8]>,
        _trace: &SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        let key_s = self.key_to_hash(key);
        if let Some(tag) = streaming_write_tag {
            if let Some(partials) = self.partial.get(&key_s) {
                if let Some(entry) = partials
                    .iter()
                    .find(|e| e.value().write_id_bytes.as_ref() == tag)
                {
                    let po = entry.value();
                    let rx = po.notify.subscribe();
                    let handler: HitHandler = Box::new(PartialHitHandler::new(po.body.clone(), rx));
                    let (internal, header) = (&po.meta.0, &po.meta.1);
                    let meta = CacheMeta::deserialize(internal, header)?;
                    MET_HITS.inc();
                    MET_STREAM_ATTACH_OK.inc();
                    return Ok(Some((meta, handler)));
                }
            }
            // Tag provided but not matched
            MET_MISSES.inc();
            MET_STREAM_ATTACH_MISS.inc();
            return Ok(None);
        }
        // No tag; defer to regular lookup
        self.lookup(key, _trace).await
    }

    async fn get_miss_handler(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        _trace: &SpanHandle,
    ) -> Result<MissHandler> {
        let key_s = self.key_to_hash(key);
        // Enforce max_partial_writes if configured
        if let Some(limit) = self.max_partial_writes {
            let current = self.partial_count.fetch_add(1, Ordering::Relaxed) + 1;
            if current > limit {
                self.partial_count.fetch_sub(1, Ordering::Relaxed);
                return Error::e_explain(
                    ErrorType::Custom("TooManyPartialWrites"),
                    format!("partial writes exceed max_partial_writes={limit}"),
                );
            }
        } else {
            // track anyway
            self.partial_count.fetch_add(1, Ordering::Relaxed);
        }
        let write_id = self.next_write_id();
        let write_id_bytes = write_id.to_be_bytes();
        let (tx, _rx) = watch::channel::<usize>(0);
        let (internal, header) = meta.serialize()?;
        let po = PartialObject {
            meta: (internal, header),
            body: Arc::new(RwLock::new(Vec::new())),
            notify: Arc::new(tx),
            write_id_bytes,
        };
        let notify = po.notify.clone();
        let body = po.body.clone();
        self.partial
            .entry(key_s.clone())
            .or_default()
            .insert(write_id, po);
        GAUGE_PARTIALS.inc();

        let hash = Self::hash_str(&key_s);
        let dir = self.key_dir(&hash);

        let mh = EdgeMissHandler {
            key: key_s,
            write_id,
            write_id_bytes,
            body,
            notify,
            cache: Arc::clone(&self.cache),
            partial: Arc::clone(&self.partial),
            meta: meta.serialize()?,
            disk_dir: dir,
            max_object_bytes: self.max_object_bytes,
            partial_count: &self.partial_count,
            max_disk_bytes: self.max_disk_bytes,
            disk_bytes_used: &self.disk_bytes_used,
            cleaned: false,
            atomic_publish: self.atomic_publish,
        };
        Ok(Box::new(mh))
    }

    async fn purge(
        &'static self,
        key: &CompactCacheKey,
        purge_type: PurgeType,
        _trace: &SpanHandle,
    ) -> Result<bool> {
        let key_s = self.compact_key_to_hash(key);
        // Best-effort size accounting before removal
        if self.max_disk_bytes.is_some() {
            // try stat on-disk body; if missing, fallback to in-memory size if present
            let mut len_to_sub: usize = 0;
            let cached_len = self.cache.get(&key_s).map(|c| c.body.len());
            let hash = Self::hash_str(&key_s);
            let dir = self.key_dir(&hash);
            let body_path = Self::body_path(&dir);
            if let Ok(meta) = fs::metadata(&body_path).await {
                len_to_sub = meta.len() as usize;
            } else if let Some(l) = cached_len {
                len_to_sub = l;
            }
            if len_to_sub > 0 {
                let after = self
                    .disk_bytes_used
                    .fetch_sub(len_to_sub, Ordering::Relaxed)
                    .saturating_sub(len_to_sub);
                GAUGE_DISK_BYTES.set(after as i64);
            }
        }
        let existed = self.cache.remove(&key_s).is_some() || self.partial.remove(&key_s).is_some();
        if existed {
            GAUGE_KEYS.set(self.cache.len() as i64);
        }
        match purge_type {
            PurgeType::Eviction => MET_PURGE_EVICTIONS.inc(),
            PurgeType::Invalidation => MET_PURGE_INVALIDATIONS.inc(),
        }
        // Best-effort disk cleanup
        let hash = Self::hash_str(&key_s);
        let dir = self.key_dir(&hash);
        let _ = fs::remove_file(Self::body_path(&dir)).await;
        let _ = fs::remove_file(Self::meta_header_path(&dir)).await;
        let _ = fs::remove_file(Self::meta_internal_path(&dir)).await;
        let _ = fs::remove_dir_all(&dir).await;
        Ok(existed)
    }

    async fn update_meta(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        _trace: &SpanHandle,
    ) -> Result<bool> {
        let key_s = self.key_to_hash(key);
        let mut updated = false;
        if let Some(mut obj) = self.cache.get_mut(&key_s) {
            obj.meta = meta.serialize()?;
            updated = true;
        }
        if let Some(partials) = self.partial.get(&key_s) {
            let (internal, header) = meta.serialize()?;
            for mut e in partials.iter_mut() {
                e.meta = (internal.clone(), header.clone());
                updated = true;
            }
        }
        // Best-effort persist meta
        let hash = Self::hash_str(&key_s);
        let dir = self.key_dir(&hash);
        let (internal, header) = meta.serialize()?;
        let _ = fs::create_dir_all(&dir).await;
        let _ = fs::write(Self::meta_internal_path(&dir), internal).await;
        let _ = fs::write(Self::meta_header_path(&dir), header).await;
        Ok(updated)
    }

    fn support_streaming_partial_write(&self) -> bool {
        true
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync + 'static) {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use pingora::cache::storage::Storage;
    use pingora::cache::trace::Span;
    use pingora::cache::{CacheKey, CacheMeta};
    use pingora::http::ResponseHeader;
    use std::time::{Duration, SystemTime};

    fn make_meta(max_age_secs: u64) -> CacheMeta {
        let created = SystemTime::now();
        let fresh_until = created + Duration::from_secs(max_age_secs);
        let header = ResponseHeader::build(200, None).unwrap();
        CacheMeta::new(fresh_until, created, 0, 0, header)
    }

    fn temp_root(label: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("edge_store_unit_{label}_{ts}"));
        p
    }

    #[tokio::test]
    async fn demotes_in_memory_after_background_persist() -> Result<()> {
        let root = temp_root("demote");
        let storage: &'static EdgeMemoryStorage =
            Box::leak(Box::new(EdgeMemoryStorage::with_disk_root(root.clone())));
        let trace = Span::inactive().handle();

        let key = CacheKey::new("ns", "/demote", "u1");
        let meta = make_meta(60);

        // Write and finish
        let mut mh = storage.get_miss_handler(&key, &meta, &trace).await?;
        mh.write_body(Bytes::from_static(b"hello world"), true).await?;
        mh.finish().await?;

        // Immediate lookup should hit (served from memory)
        assert!(storage.lookup(&key, &trace).await?.is_some());

        // In-memory entry should be present right after finish
        let key_s = storage.key_to_hash(&key);
        assert!(storage.cache.contains_key(&key_s));

        // Wait until files are visible on disk
        let mut tries = 0;
        loop {
            let hash = EdgeMemoryStorage::hash_str(&storage.key_to_hash(&key));
            let dir = storage.key_dir(&hash);
            if fs::try_exists(EdgeMemoryStorage::body_path(&dir)).await.unwrap_or(false)
                && fs::try_exists(EdgeMemoryStorage::meta_header_path(&dir))
                    .await
                    .unwrap_or(false)
                && fs::try_exists(EdgeMemoryStorage::meta_internal_path(&dir))
                    .await
                    .unwrap_or(false)
            {
                break;
            }
            tries += 1;
            if tries > 100 {
                panic!("persist did not materialize files");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Verify in-memory entry is removed after persist completes
        let mut tries = 0;
        loop {
            // Lookup should still be a hit throughout
            assert!(storage.lookup(&key, &trace).await?.is_some());
            if !storage.cache.contains_key(&key_s) {
                break; // demoted
            }
            tries += 1;
            if tries > 200 {
                panic!("in-memory entry not demoted after persist");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(())
    }

    fn get_counter_value(name: &str) -> f64 {
        for mf in prometheus::gather() {
            if mf.get_name() == name {
                let mut sum = 0.0;
                for m in mf.get_metric() {
                    if m.has_counter() {
                        sum += m.get_counter().get_value();
                    }
                }
                return sum;
            }
        }
        0.0
    }

    #[tokio::test]
    async fn demotion_increments_metric() -> Result<()> {
        use bytes::Bytes;
        let root = temp_root("demote_metric");
        let storage: &'static EdgeMemoryStorage =
            Box::leak(Box::new(EdgeMemoryStorage::with_disk_root(root.clone())));
        let trace = Span::inactive().handle();
        let key = CacheKey::new("ns", "/dmetric", "u1");
        let meta = make_meta(60);

        let before = get_counter_value("edge_cache_demotions_total");

        let mut mh = storage.get_miss_handler(&key, &meta, &trace).await?;
        mh.write_body(Bytes::from_static(b"abc"), true).await?;
        mh.finish().await?;

        // Wait until demotion happens (in-memory removed)
        let key_s = storage.key_to_hash(&key);
        let mut tries = 0;
        loop {
            if !storage.cache.contains_key(&key_s) {
                break;
            }
            tries += 1;
            if tries > 200 {
                panic!("demotion did not complete");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let after = get_counter_value("edge_cache_demotions_total");
        assert!(after >= before + 1.0, "demotion counter should increase");
        Ok(())
    }

     #[tokio::test]
    async fn atomic_publish_persists_and_no_tmp_left() -> Result<()> {
        use tokio::fs as tfs;
        let root = temp_root("atomic");
        let storage: &'static EdgeMemoryStorage = Box::leak(Box::new(
            EdgeMemoryStorageBuilder::new()
                .with_disk_root(root.clone())
                .with_atomic_publish(Some(true))
                .build(),
        ));
        let trace = Span::inactive().handle();
        let key = CacheKey::new("ns", "/atomic", "u1");
        let meta = make_meta(60);

        // Write and finish
        let mut mh = storage.get_miss_handler(&key, &meta, &trace).await?;
        mh.write_body(Bytes::from_static(b"hello atomic"), true).await?;
        mh.finish().await?;

        // Wait until final files are visible on disk and ensure tmp dir is gone
        let hash = EdgeMemoryStorage::hash_str(&storage.key_to_hash(&key));
        let dir = storage.key_dir(&hash);
        let tmp_dir = dir.with_extension("tmp");

        let mut tries = 0;
        loop {
            let body_ok = tfs::try_exists(EdgeMemoryStorage::body_path(&dir))
                .await
                .unwrap_or(false);
            let mh_ok = tfs::try_exists(EdgeMemoryStorage::meta_header_path(&dir))
                .await
                .unwrap_or(false);
            let mi_ok = tfs::try_exists(EdgeMemoryStorage::meta_internal_path(&dir))
                .await
                .unwrap_or(false);
            if body_ok && mh_ok && mi_ok {
                break;
            }
            tries += 1;
            if tries > 100 {
                panic!("atomic publish did not materialize final files");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // tmp dir should not exist once rename completed
        let tmp_exists = tfs::try_exists(&tmp_dir).await.unwrap_or(false);
        assert!(!tmp_exists, "temporary dir should be removed after atomic rename");

        // New instance should be able to hit from disk
        let storage2: &'static EdgeMemoryStorage =
            Box::leak(Box::new(EdgeMemoryStorage::with_disk_root(root)));
        let hit = storage2.lookup(&key, &trace).await?;
        assert!(hit.is_some());
        Ok(())
    }
}
