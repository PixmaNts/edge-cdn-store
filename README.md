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
edge-cdn-cache-atomic-publish: true             # planned
edge-cdn-cache-io-uring-enabled: false          # planned
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
  cargo run --example cdn_server
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

- Atomic publish (write `*.part`, fsync file + dir, rename) and optional `io_uring` backend.
- Improve capacity accounting accuracy and add disk read/write metrics.
- Tiered cache adapter to add a shared/distributed layer.
- Startup indexer to seed eviction manager and rebuild in-memory state.
