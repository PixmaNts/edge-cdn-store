# edge-cdn-store

A CDN cache storage backend implementation for the `pingora_cache` library, developed as part of a Wasmer technical evaluation.

## Overview

This project implements a storage layer for CDN caching that integrates with Cloudflare's `pingora_cache` framework. The implementation focuses on high-concurrency scenarios, low memory usage, and pluggable observability features.

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
edge-cdn-cache-max-disk-bytes: 1073741824       # optional; enforced on finish (approximate)
edge-cdn-cache-max-object-bytes: 104857600      # optional; enforced during write/finish
edge-cdn-cache-max-partial-writes: 64           # optional; enforced on admission
edge-cdn-cache-atomic-publish: true             # enabled (atomic publish)
edge-cdn-cache-io-uring-enabled: false          # optional (uses io_uring for disk reads)
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

The example exposes a Prometheus HTTP server; you can also scrape these from any app that links the library.

## Tests

Run the full suite:

```
cargo test
```

Included tests:
- Integration tests (write→hit→reload→purge, streaming partials, seek).
- Config parser tests (full/partial/unknown YAML).

## Roadmap

- Expand `io_uring` backend to writes and atomic publish.

## Example Configuration

You can configure the example via Pingora’s YAML (using our namespaced keys) and/or environment variables. The example reads the YAML passed to `pingora::Opt` (`-c path.yaml`), then applies env overrides.

YAML keys (top-level):

```
edge-cdn-cache-disk-root: "/var/lib/edge_store"
edge-cdn-cache-max-disk-bytes: 1073741824
edge-cdn-cache-max-object-bytes: 104857600
edge-cdn-cache-max-partial-writes: 64
edge-cdn-cache-atomic-publish: true
edge-cdn-cache-io-uring-enabled: true
```

Env overrides (optional):

- `EDGE_STORE_DIR`: path for on-disk data
- `EDGE_MAX_DISK_BYTES`: integer bytes
- `EDGE_MAX_OBJECT_BYTES`: integer bytes
- `EDGE_MAX_PARTIAL_WRITES`: integer
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
- Improve capacity accounting accuracy; expand disk metrics & tracing.
- Hot-cache layer for small objects alongside disk.
- Startup indexer to seed eviction manager and rebuild in-memory state.
