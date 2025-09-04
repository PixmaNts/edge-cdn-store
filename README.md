# edge-cdn-store

A CDN cache storage backend implementation for the `pingora_cache` library, developed as part of a Wasmer technical evaluation.

## Overview

This project implements a storage layer for CDN caching that integrates with Cloudflare's `pingora_cache` framework. The implementation focuses on high-concurrency scenarios, low memory usage, and pluggable observability features.

Status at a glance:
- Implements `pingora_cache::Storage` with async Tokio I/O and streaming partial writes.
- Persists to disk with atomic publish enabled by default (tmp + fsync + atomic rename). Non-atomic finalize is available but less durable.
- Optional `io_uring` support for disk reads and streaming writes (experimental; finalize still fsyncs via Tokio). See notes below.
- Prometheus metrics for hits/misses, eviction/invalidation, streaming attach, key/partial gauges, and disk I/O histograms.

## Features

- High-performance concurrent metadata operations
- Tokio async runtime integration
- Memory-efficient design for scale
- Pluggable observability (Prometheus metrics, logging, health checks)
- Integration with `pingora_cache` EvictionManager (example wired with simple LRU)
- Configurable storage behavior via Pingora YAML (namespaced keys)
- Designed for tiered cache architecture

## Requirements

- Rust 2024 edition
- Tokio runtime
- pingora-cache library

## Architecture

The implementation provides a custom storage backend for `pingora_cache` by implementing the `Storage` trait. See `design/` directory for detailed proposal and requirements.

io_uring support (experimental):
- When enabled, disk reads and streaming body writes use `io_uring` with graceful fallback to Tokio.
- Finalize (fsync + rename) currently uses Tokio APIs; this is acceptable but not fully uring-native yet.
- Integration tests for `io_uring` exist and are marked `#[ignore]` as they depend on host kernel/capabilities.

### Disk Layout (MVP)

`<root>/<hh>/<hh>/<full_hash>/`
- `body.bin`
- `meta_internal.bin`
- `meta_header.bin`

Meta is persisted using `CacheMeta::serialize()`/`deserialize()` in (internal, header) order.

## Configuration

The library can parse Pingora's YAML directly for storage-specific keys. Unknown keys are ignored by Pingora and by our parser.

Add the following keys to your Pingora YAML (top-level):

```
edge-cdn-cache-disk-root: "/var/lib/edge_store"
edge-cdn-cache-max-disk-bytes: 1073741824       # optional; enforced on finish (approximate, body-only)
edge-cdn-cache-max-object-bytes: 104857600      # optional; enforced during write and on finish
edge-cdn-cache-max-partial-writes: 64           # optional; enforced on admission
edge-cdn-cache-max-in-mem-partial-bytes: 1048576 # optional; per-object tail kept in memory for partial readers
edge-cdn-cache-atomic-publish: true             # default: true; write meta + fsync + atomic rename
edge-cdn-cache-io-uring-enabled: false          # optional; enable uring for reads + streaming writes (experimental)
```

Programmatic usage:

```rust
use edge_cdn_store::{EdgeMemoryStorage, EdgeStoreConfig};
use pingora::server::configuration::Opt;

let opt = Opt::parse_args();
let cfg = opt.conf.as_deref()
    .map(EdgeStoreConfig::from_yaml_file)
    .unwrap_or_default();
let storage = EdgeMemoryStorage::from_config(&cfg);
```

Environment fallback for disk root: `EDGE_STORE_DIR` (default: `edge_store_data`).

## Example (Pingora Proxy)

Run the demo proxy with cache + eviction + metrics:

```
RUST_LOG=info EDGE_EVICTION_LIMIT_BYTES=268435456 \
  cargo run --example cdn_server -- -c examples/pingora.yaml
```

- Proxy: `http://localhost:8080`
- Prometheus metrics: `http://127.0.0.1:6192/metrics`

The example injects headers for visibility:
- `x-cache-status`: hit | miss | expired | stale | ...
- `x-total-time-ms`: end-to-end time in milliseconds

## Prometheus Metrics (Library)

Exported by the library (via global registries):
- `edge_cache_hits_total`
- `edge_cache_misses_total`
- `edge_cache_evictions_total`
- `edge_cache_invalidations_total`
- `edge_cache_stream_attach_total`
- `edge_cache_stream_attach_miss_total`
- `edge_cache_keys` (gauge)
- `edge_cache_partial_writes` (gauge)
- `edge_cache_disk_bytes` (gauge)
- `edge_cache_atomic_publish_enabled` (gauge; 1 if enabled)
- `edge_cache_io_uring_enabled` (gauge; 1 if enabled)

The example exposes a Prometheus HTTP server; you can also scrape these from any app that links the library.

## Tests

Run the full suite:

```
cargo test
```

Included tests:
- Integration tests (write→hit→reload→purge, streaming partials, seek).
- Capacity tests (disk/object caps, partial writer limits, regression for counter underflow).
- io_uring parity tests exist and are `#[ignore]` by default (environment-dependent).

## Roadmap / TODO

- Durability: Add fsync of meta and parent dir in non-atomic finalize path (or recommend atomic publish by default).
- Eviction coordination: On `max_disk_bytes` exceed at finish, attempt purge via EvictionManager before rejecting finishes.
- Startup indexer: Scan disk to seed eviction state and rebuild in-memory state; compute disk usage at boot.
- io_uring hardening: Feature-gate; expand finalize ops to be uring-native; unignore tests when supported; add a gauge to indicate active uring mode.
- Metrics: Expand disk/capacity metrics and tracing spans; include error counters for write/read failures and finalize outcomes.
- Hot-cache layer: Optional small-object in-memory layer alongside disk for very hot items.
- Documentation: Keep README and example YAML aligned; clearly tag experimental features (io_uring).

## Example Configuration

You can configure the example via Pingora’s YAML (using our namespaced keys) and/or environment variables. The example reads the YAML passed to `pingora::Opt` (`-c path.yaml`), then applies env overrides.

YAML keys (top-level):

```
edge-cdn-cache-disk-root: "/var/lib/edge_store"
edge-cdn-cache-max-disk-bytes: 1073741824
edge-cdn-cache-max-object-bytes: 104857600
edge-cdn-cache-max-partial-writes: 64
edge-cdn-cache-max-in-mem-partial-bytes: 1048576
edge-cdn-cache-atomic-publish: true
edge-cdn-cache-io-uring-enabled: true
```

Env overrides (optional):

- `EDGE_STORE_DIR`: path for on-disk data
- `EDGE_MAX_DISK_BYTES`: integer bytes
- `EDGE_MAX_OBJECT_BYTES`: integer bytes
- `EDGE_MAX_PARTIAL_WRITES`: integer
- `EDGE_MAX_IN_MEM_PARTIAL_BYTES`: integer (per-object tail kept in memory)
- `EDGE_ATOMIC_PUBLISH`: 0/1/true/false
- `EDGE_IO_URING`: 0/1/true/false

Examples:

```
# Enable io_uring reads with atomic publish and custom dir
EDGE_IO_URING=1 EDGE_ATOMIC_PUBLISH=1 EDGE_STORE_DIR=/tmp/edge_store \
  cargo run --example cdn_server -- -c examples/pingora.yaml

# Minimal (YAML only)
cargo run --example cdn_server -- -c examples/pingora.yaml
```

## License

This project is licensed under either of

- MIT license (see `LICENSE-MIT`), or
- Apache License, Version 2.0 (see `LICENSE-APACHE`),

at your option.

## Links

- GitHub repository: https://github.com/PixmaNts/edge-cdn-store
- Changelog: `CHANGELOG.md`
- Design: `design/requirement.md`, `design/proposal.md`, `design/architecture_diagrams.md`
- Next steps / roadmap details: `design/next.md`
