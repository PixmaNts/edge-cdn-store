# Changelog

All notable changes to this project will be documented in this file.

The format is based on Keep a Changelog, and this project adheres to Semantic Versioning.

## [0.1.0] - 2025-09-04

Initial MVP release of edge-cdn-store.

- Implements `pingora_cache::Storage` with async Tokio I/O.
- Streaming partial writes with reader attach by tag; bounded in‑memory tail with disk fallback.
- On-disk persistence with per-key directory layout; atomic publish enabled by default (tmp + fsync + rename).
- Optional, experimental `io_uring` path for disk reads and streaming writes with graceful fallback.
- Prometheus metrics: hits/misses, eviction/invalidation, stream attach (ok/miss), key/partial gauges, disk bytes gauge, disk read/write histograms, feature gauges (atomic publish, io_uring).
- Capacity controls: `max_object_bytes`, `max_disk_bytes` (approximate), `max_partial_writes`.
- Purge, update_meta, range/seek on hits; integration tests and capacity tests.
- Example Pingora proxy with LRU eviction manager and Prometheus endpoint.

[0.1.0]: https://example.com/edge-cdn-store/releases/tag/v0.1.0

