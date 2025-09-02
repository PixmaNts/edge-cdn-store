use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashMap;
use pingora::Result;
use pingora::cache::key::{CacheHashKey, CompactCacheKey};
use pingora::cache::storage::{
    HandleHit, HandleMiss, HitHandler, MissFinishType, MissHandler, PurgeType, Storage,
};
use pingora::cache::trace::SpanHandle;
use pingora::cache::{CacheKey, CacheMeta};
use pingora::{ErrorType, OrErr};
use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{RwLock, watch};

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
    pub fn default_instance() -> Self { Self::new() }
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
        }
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
    fn default() -> Self { Self::new() }
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
}

#[async_trait]
impl HandleMiss for EdgeMissHandler {
    async fn write_body(&mut self, data: Bytes, eof: bool) -> Result<()> {
        let mut body = self.body.write().await;
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

        let cache_object = CacheObject {
            meta: self.meta.clone(),
            body: Arc::new(complete_body),
        };

        // Insert into complete cache
        self.cache.insert(self.key.clone(), cache_object);

        // Persist to disk in the background (best-effort for MVP)
        let dir = self.disk_dir.clone();
        let (internal, header) = self.meta.clone();
        let body_for_disk = self.cache.get(&self.key).map(|c| c.body.clone());
        tokio::spawn(async move {
            if let Some(body_arc) = body_for_disk {
                let _ = persist_to_disk(dir, &internal, &header, &body_arc).await;
            }
        });

        // Remove from partial cache (avoid holding outer map guard while removing key)
        if let Some(partial_map) = self.partial.get(&self.key) {
            partial_map.remove(&self.write_id);
            let empty = partial_map.is_empty();
            drop(partial_map);
            if empty {
                self.partial.remove(&self.key);
            }
        }

        Ok(MissFinishType::Created(body_size))
    }

    fn streaming_write_tag(&self) -> Option<&[u8]> {
        Some(&self.write_id_bytes)
    }
}

impl Drop for EdgeMissHandler {
    fn drop(&mut self) {
        // Clean up partial cache entry if handler is dropped without finishing
        if let Some(partial_map) = self.partial.get(&self.key) {
            partial_map.remove(&self.write_id);
            let empty = partial_map.is_empty();
            drop(partial_map);
            if empty {
                self.partial.remove(&self.key);
            }
        }
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

// Read meta and body from disk and construct CacheObject
async fn load_from_disk(dir: &Path) -> Result<Option<CacheObject>> {
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
        let mut bf = fs::File::open(body_path)
            .await
            .or_err(ErrorType::FileOpenError, "open body file")?;
        let mut body = Vec::new();
        bf.read_to_end(&mut body)
            .await
            .or_err(ErrorType::FileReadError, "read body file")?;

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

        Ok(Some(CacheObject {
            meta: (mi, mh),
            body: Arc::new(body),
        }))
    } else {
        Ok(None)
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
        let mut buf = vec![0u8; 64 * 1024];
        let n = self
            .file
            .read(&mut buf)
            .await
            .or_err(ErrorType::FileReadError, "disk hit read")?;
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
                return Ok(Some((meta, handler)));
            }
        }

        // Load from disk if exists
        let hash = Self::hash_str(&key_s);
        let dir = self.key_dir(&hash);
        if let Some(obj) = load_from_disk(&dir).await? {
            let meta = CacheMeta::deserialize(&obj.meta.0, &obj.meta.1)?;
            let body = obj.body.clone();
            self.cache.insert(key_s.clone(), obj);
            let handler: HitHandler = Box::new(CompleteHitHandler::new(body));
            return Ok(Some((meta, handler)));
        }

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
                    return Ok(Some((meta, handler)));
                }
            }
            // Tag provided but not matched
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
        };
        Ok(Box::new(mh))
    }

    async fn purge(
        &'static self,
        key: &CompactCacheKey,
        _purge_type: PurgeType,
        _trace: &SpanHandle,
    ) -> Result<bool> {
        let key_s = self.compact_key_to_hash(key);
        let existed = self.cache.remove(&key_s).is_some() || self.partial.remove(&key_s).is_some();
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
