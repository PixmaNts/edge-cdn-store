# WARP: Edge CDN Storage Backend for Pingora Cache

## Introduction

This proposal outlines the design and implementation of `edge-cdn-store`, a CDN cache storage backend that implements the `pingora_cache::Storage` trait. The project aims to provide a caching solution for edge computing scenarios where low latency, high throughput, and data persistence are important.

The architecture uses an in-memory `DashMap` for managing metadata and in-progress writes, coupled with a persistent on-disk storage layer. The current MVP uses `tokio::fs` for disk I/O; `io_uring` remains a planned enhancement. The design relies on the operating system's kernel page cache to keep hot data fast without an explicit in-memory data cache.

## Motivation

### Technical Benefits

**Performance Requirements:**

- **High Concurrency**: Edge nodes must handle many simultaneous requests. The `DashMap` manages metadata and in-progress writes, helping with concurrency.
- **Low Latency**: Cache lookups and retrievals must be fast. For new data, latency is low due to in-memory buffering during network access. For existing hot data, low latency is provided by the kernel page cache.
- **Memory Efficiency**: The explicit in-memory component is for metadata and in-progress data, not a full data cache. This reduces application memory footprint, relying on the kernel for data caching.
- **Persistence & Scale**: Data persists across restarts and scales beyond available RAM via the `io_uring` disk layer.
- **Streaming Support**: Large files need progressive download/serving. The system supports streaming from in-memory buffers during writes and from disk reads.
- **Disk I/O**: `io_uring` provides an asynchronous interface for disk operations. This, combined with the kernel page cache, optimizes disk access.

**Current Gaps:**

- Solutions relying solely on explicit in-memory caches are limited by RAM and lack persistence.
- Traditional blocking disk I/O can introduce latency.
- Existing caching solutions may not optimize for partial content streaming during initial data ingestion.

### Product Impact

**Business Value:**

- **Cost Reduction**: Reduced application memory footprint and efficient disk usage.
- **User Experience**: Faster content delivery, especially for new and hot content.
- **Reliability**: Data persistence on disk ensures cache availability after service restarts.
- **Operations**: Metrics and health checks enable monitoring.
- **Scalability**: The architecture supports scaling edge nodes by efficiently managing data on disk and leveraging kernel caching.

## Explanation

### Architecture Overview (MVP)

`EdgeMemoryStorage` manages metadata and in-progress writes, and interacts with the disk:

```rust
pub struct EdgeMemoryStorage {
    cache: Arc<DashMap<String, CacheObject>>,    // complete objects ready to serve
    partial: Arc<DashMap<String, DashMap<u64, PartialObject>>>, // in-progress writes
    write_counter: AtomicU64,
    disk_root: PathBuf,
}

struct CacheObject { meta: (Vec<u8>, Vec<u8>), body: Arc<Vec<u8>> }
struct PartialObject {
    meta: (Vec<u8>, Vec<u8>),
    body: Arc<RwLock<Vec<u8>>>,
    notify: Arc<watch::Sender<usize>>, // available bytes and EOF marker
    write_id_bytes: [u8; 8],           // for streaming write tag matching
}
```

When a request misses, data is loaded from the origin. It is progressively buffered in memory (managed by `metadata_and_in_progress_cache`) while an asynchronous write to disk via `io_uring` is initiated. During this time, subsequent requests for the same resource are served from the in-memory buffer. Once the disk write completes, the data is served directly from disk, relying on the kernel page cache for in-memory performance.

### Core Design Decisions

**1. In-Memory `DashMap` (Metadata & In-Progress Cache)**

- **Purpose**: To manage metadata for all cached items and to buffer data for in-progress writes from origin.
- **Structure**: `DashMap<String, CacheObject>` where `CacheObject::InProgress` holds the data being streamed from origin, and `CacheObject::OnDisk` holds metadata for content already on disk.
- **Benefits**: Lock-free concurrent reads/writes for metadata and in-progress data, allowing fast access and updates.
- **Role in Cache Misses**: When a request misses, a new `InProgress` entry is created. This entry buffers data as it arrives from the origin, allowing immediate serving of subsequent requests for the same resource while the full content is still being downloaded and written to disk.

**2. On-Disk Storage (Persistent Cache)**

- **Purpose**: Provide persistence and scale beyond RAM.
- **Technology (MVP)**: `tokio::fs` async I/O. `io_uring` planned as an opt-in backend.
- **Structure**: Per-key directory layout with: `body.bin`, `meta_internal.bin`, `meta_header.bin`.
- **Serialization**: `CacheMeta::serialize()`/`deserialize()` with correct (internal, header) ordering.

### Current Status (MVP)

- Storage trait implemented: `lookup`, `lookup_streaming_write`, `get_miss_handler`, `update_meta`, `purge`, and `support_streaming_partial_write`.
- Streaming writes: concurrent in-progress per key; readers can attach via a tag returned by the miss handler.
- Persistence: on finish, completed objects are persisted in the background; storage reloads from disk (and repopulates memory) on demand.
- Example proxy: Pingora example wired to use the storage; adds `x-cache-status` and `x-total-time-ms` headers.
- Tests: integration tests for miss→hit→disk reload and streaming partial read; fixed a DashMap guard removal deadlock and EOF slicing bug.

### Disk Layout

`<root>/<hh>/<hh>/<full_hash>/` with files:
- `body.bin`
- `meta_internal.bin`
- `meta_header.bin`
  - Asynchronous reading of cache body data from disk.
  - Asynchronous writing of new cache entries to disk.
  - Deletion of purged entries.
- **Reliance on Kernel Page Cache**: This design explicitly relies on the operating system's kernel page cache to keep frequently accessed disk blocks in memory. This provides the in-memory performance benefits for hot data without the application managing a separate data cache.

**3. Cache Coherence and Data Flow**

- **Read Path**:
    1. `lookup` checks `metadata_and_in_progress_cache`.
    2. If `CacheObject::InProgress` is found, serve data directly from its in-memory buffer.
    3. If `CacheObject::OnDisk` is found, initiate an `io_uring` read from disk. The kernel page cache will serve hot data directly from memory.
    4. If no entry is found in `metadata_and_in_progress_cache`, it's a cache miss.
- **Write Path (on Cache Miss)**:
    1. Request misses, data is loaded from the origin.
    2. A `CacheObject::InProgress` entry is created in `metadata_and_in_progress_cache` to buffer the incoming data.
    3. An asynchronous `io_uring` write to disk is initiated for the incoming data.
    4. While the disk write is in progress, subsequent requests for the same resource are served from the `InProgress` entry's in-memory buffer.
    5. Once the `io_uring` disk write completes, the `InProgress` entry in `metadata_and_in_progress_cache` is converted to a `CacheObject::OnDisk` entry, indicating the data is now persistently stored.
- **Eviction**: An `EvictionManager` will manage entries in `metadata_and_in_progress_cache` and trigger deletion of corresponding files on disk based on policies (e.g., LRU, LFU).

**4. Streaming Coordination System**

- `tokio::sync::watch` channels will coordinate readers/writers for partial content, streaming data from either the in-memory buffer (for `InProgress` entries) or from `io_uring` disk read streams.
- This allows multiple readers to consume data as it's being written or read from disk.

### Implementation Strategy

**Phase 1: Core Storage Trait Implementation**

- **`lookup`**: Implement logic to check `metadata_and_in_progress_cache` for both `InProgress` and `OnDisk` entries, initiating `io_uring` reads for the latter.
- **`get_miss_handler`**: Create a `MissHandler` that manages the `InProgress` state in `metadata_and_in_progress_cache` and initiates `io_uring` writes.
- **`purge`**: Implement deletion logic for `metadata_and_in_progress_cache` entries and corresponding files on disk.
- **`update_meta`**: Update metadata in `metadata_and_in_progress_cache` for `OnDisk` entries.

**Phase 2: `IoUringDiskStorage` Module**

- Develop a module for `IoUringDiskStorage` that encapsulates `io_uring` interactions for file creation, reading, writing, and deletion.
- Handle `io_uring` setup, submission, and completion queues.

**Phase 3: Handler Implementations**

- **`CompleteHitHandler`**: Adapt to serve content from `InProgress` buffers or `io_uring` read streams.
- **`PartialHitHandler`**: Continue to stream partial content.
- **`EdgeMissHandler`**: Coordinate cache writes, managing the `InProgress` state and `io_uring` writes.

**Phase 4: Observability & Operations**

- Extend Prometheus metrics to cover cache hit/miss rates, memory usage (for `DashMap`), disk usage, `io_uring` queue depths, and concurrent operations.
- Health check endpoints will monitor the status and performance of the cache.

### Key Technical Features

**Concurrency Model:**

- Lock-free reads/writes for `DashMap` (metadata and in-progress data).
- Asynchronous, non-blocking disk I/O via `io_uring`.
- Supports many concurrent operations.

**Memory Management:**

- Explicit in-memory usage is limited to metadata and in-progress data buffering.
- Relies on kernel page cache for hot data on disk.
- Efficient handling of `io_uring` buffers.

**Streaming Capabilities:**

- Partial content serving from in-memory buffers (for in-progress writes) or disk.
- Range request support for HTTP caching, handled by `io_uring` for disk reads.
- Coordinated streaming between multiple readers.

## Drawbacks & Alternatives

### Drawbacks

**Increased Complexity:**

- Managing in-progress states and coordinating between `DashMap` and `io_uring` adds complexity.
- Data consistency and synchronization between in-progress buffers and disk need careful handling.
- Debugging issues can be challenging.

**Resource Overhead:**

- MVP uses `tokio::fs` and works on stable kernels.
- `io_uring` (planned) requires recent kernels and careful setup.

**Development Time:**

- Implementing an `io_uring` based storage layer and its integration will require development effort.

### Alternatives Considered

**1. Single-Layer In-Memory Only:**

- **Pros**: Simple to implement, fast for hot data.
- **Cons**: Limited by RAM, no persistence, not for large datasets.

**2. Single-Layer Disk-Based (Traditional Blocking I/O):**

- **Pros**: Persistence, scales to large datasets.
- **Cons**: Performance bottlenecks due to blocking I/O, higher latency for cache hits, less efficient for high concurrency.

**3. External Distributed Cache (e.g., Redis, Memcached):**

- **Pros**: Scales, distributed caching.
- **Cons**: Network latency overhead, additional operational complexity, not for a primary local cache.

**4. Memory-Mapped Files (for Disk Layer):**

- **Pros**: OS-level memory management.
- **Cons**: Complex partial update semantics, platform-specific behavior, less control over I/O compared to `io_uring`, potential for page faults to introduce latency.

### Risk Mitigation

**Performance Validation:**

- Benchmarks comparing the approach against alternatives.
- Load testing under realistic traffic patterns.
- Memory and disk usage profiling.

**Correctness Assurance:**

- Unit tests for each component and their interactions.
- Integration tests with the `pingora_cache` framework.
- Fuzzing tests for edge cases.

**Phased Implementation:**

- Phase 1 (done): In-memory management + tokio::fs persistence, streaming partials, tests, Pingora example.
- Phase 2 (next): `io_uring` disk I/O behind a feature/env flag and atomic publish (`*.part` + fsync + rename).
- Phase 3: Eviction manager integration, metrics/observability, performance validation.

## Summary & Conclusion

The `EdgeMemoryStorage` architecture, using in-memory `DashMap` for metadata and in-progress writes with on-disk persistence (tokio::fs in MVP, `io_uring` planned), provides a CDN caching solution. It balances:

1. **Speed**: Low latency for new data (in-progress) and hot data (kernel page cache).
2. **Persistence & Scale**: Storage for large datasets on disk.
3. **Efficiency**: Asynchronous disk I/O via `io_uring`.
4. **Concurrency**: Supports many simultaneous operations.
5. **Observability**: Metrics for cache activity.

This design addresses requirements for an edge CDN cache.

**Decision Criteria for Acceptance:**

- Correctness validated by tests and example (hits/misses, reload on restart, streaming partial reads).
- Performance benchmarks show improved hit latency and acceptable miss latency.
- Memory and disk usage remain within operational limits.
- Code review approval focusing on correctness and architectural soundness; later, `io_uring` safety.

**Next Steps:**

1. Introduce `io_uring` backend behind a feature/env flag; add atomic publish with fsync + rename.
2. Add metrics (hits/misses, disk ops, write/read timings) and optional eviction manager.
3. Expand tests (range/seek, concurrent writers/readers, crash recovery) and run basic benchmarks.
4. Improve example (HTTPS/H2 upstream, warm-up, knobs for storage paths and flags).
