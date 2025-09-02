use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashMap;
use pingora::cache::storage::{HandleHit, HandleMiss, HitHandler, MissHandler, PurgeType, Storage};
use pingora::cache::{CacheKey, CacheMeta};
use pingora::cache::key::{CacheHashKey, CompactCacheKey};
use pingora::cache::trace::SpanHandle;
use pingora::Result;
use std::any::Any;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{watch, RwLock};

/// DashMap-based in-memory storage for pingora cache
pub struct EdgeMemoryStorage {
    /// Complete cached objects ready to serve
    pub cache: DashMap<String, CacheObject>,
    
    /// Partial writes in progress - supports concurrent streaming writes
    pub partial: DashMap<String, DashMap<u64, PartialObject>>,
    
    /// Atomic counter for unique write IDs
    pub write_counter: AtomicU64,
}

/// A complete cached object ready for immediate serving
struct CacheObject {
    /// Serialized cache metadata (header + internal metadata)
    meta: (Vec<u8>, Vec<u8>),
    /// Complete response body
    body: Arc<Vec<u8>>,
}

/// A partial cached object currently being written
struct PartialObject {
    /// Serialized cache metadata
    meta: (Vec<u8>, Vec<u8>),
    /// Growing response body (protected by RwLock for concurrent access)
    body: Arc<RwLock<Vec<u8>>>,
    /// Notifies readers when new bytes are available
    notify: Arc<watch::Sender<usize>>,
}

impl EdgeMemoryStorage {
    /// Create a new EdgeMemoryStorage instance
    pub fn new() -> Self {
        Self {
            cache: DashMap::new(),
            partial: DashMap::new(),
            write_counter: AtomicU64::new(0),
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
                format!("Seek start {} exceeds body length {}", start, self.body.len()),
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
            
            if available_bytes > self.position {
                // New data is available
                let body = self.body.read().await;
                let chunk = Bytes::copy_from_slice(&body[self.position..available_bytes]);
                self.position = available_bytes;
                return Some(chunk);
            } else if available_bytes == usize::MAX {
                // Special marker for EOF - read any remaining data
                if self.position < available_bytes {
                    let body = self.body.read().await;
                    if self.position < body.len() {
                        let chunk = Bytes::copy_from_slice(&body[self.position..]);
                        self.position = body.len();
                        return Some(chunk);
                    }
                }
                return None;
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

    async fn finish(self: Box<Self>) -> Result<pingora::cache::storage::MissFinishType> {
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
        
        // Remove from partial cache
        if let Some(partial_map) = self.partial.get(&self.key) {
            partial_map.remove(&self.write_id);
            if partial_map.is_empty() {
                self.partial.remove(&self.key);
            }
        }
        
        Ok(pingora::cache::storage::MissFinishType::Created(body_size))
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
            if partial_map.is_empty() {
                self.partial.remove(&self.key);
            }
        }
    }
}
