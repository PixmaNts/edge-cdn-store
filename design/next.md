# Next Steps: Known Issues, Fix Ideas, and Benchmarks

This document summarizes the current MVP’s limitations, proposes practical fixes and future enhancements, and outlines a benchmarking plan to validate performance and correctness.

## Current Limitations and Issues

- Durability (non-atomic finalize)
  - Issue: The non-atomic finalize path writes meta files without fsyncing files or parent directories, making it less crash-safe.
  - Impact: After a crash or power loss, on-disk state may be inconsistent even if data was written.
  - Status: Atomic publish is now default-on (tmp + fsync + atomic rename), which is safer.

- Disk capacity and eviction coordination
  - Issue: When `max_disk_bytes` would be exceeded at finish, the write is rejected outright.
  - Impact: User-visible write failures even if eviction could free space.
  - Status: Acceptable for MVP; better to try eviction first.

- Startup indexer and state rebuild
  - Issue: No background indexer exists to rebuild in-memory state (e.g., disk usage, warming eviction manager) on boot.
  - Impact: Cold start lacks accurate bytes accounting and eviction hints; orphan `*.tmp` dirs may remain after crashes.

- io_uring maturity and feature gating
  - Issue: io_uring is experimental; only read path and streaming-body write path use it; finalize still uses Tokio fsync/rename.
  - Impact: Mixed I/O paths and environment dependence; tests are `#[ignore]`.

- Capacity accounting (approximate)
  - Issue: `max_disk_bytes` tracks body bytes only and is adjusted best-effort on purge.
  - Impact: Minor drift vs. real usage; meta sizes ignored.

- Partial streaming behavior
  - Issue: Partial readers cannot seek; tail-trimming relies on a per-object buffer and base offset; chunk size and thresholds are fixed.
  - Impact: Less flexible range handling for in-progress content; tuning may be needed.

- Error handling and backpressure
  - Issue: Fixed timeouts for io_uring ops; limited categorization of failures; no counters for finalize success/failure.
  - Impact: Hard to diagnose intermittent I/O issues; limited ops visibility.

- Observability gaps
  - Issue: Metrics are good but can be expanded (e.g., per-op failure counters, finalize results, open file cache stats for io_uring).
  - Impact: Harder to debug under load.

- Crash consistency and cleanup
  - Issue: Pre-rename crashes can leave `*.tmp` dirs; non-atomic path less safe; no boot-time scrubbing.
  - Impact: Orphaned data and quota drift.

- Disk layout and scalability
  - Issue: Deep per-key directory layout may stress inodes for very high key counts.
  - Impact: Operational considerations for very large caches.

- Cross-platform considerations
  - Issue: io_uring is Linux-only; current code is tuned for Linux.
  - Impact: Portability limits.

## Fix Ideas and Enhancements

- Durability improvements
  - Make atomic publish the recommended (and current default) mode for production.
  - Add fsyncs to the non-atomic finalize path for meta files and parent directory; document trade-offs.

- Eviction coordination on capacity
  - When finish would exceed `max_disk_bytes`, attempt to trigger eviction (via the provided EvictionManager) before rejecting.
  - Expose a hook/metric for “evict-before-admit” attempts and outcomes.

- Startup indexer and housekeeping
  - On boot, scan the disk root to:
    - Compute `disk_bytes_used` accurately (body + optionally meta).
    - Seed the eviction manager with discovered keys and sizes.
    - Remove orphan `*.tmp` dirs and incomplete entries.
  - Run indexing on a background task; expose progress metrics.

- io_uring hardening
  - Feature-gate io_uring (Cargo feature and config flag) and add an explicit gauge for active status (added).
  - Extend finalize to uring (fsync ops, rename if applicable; otherwise keep mixed approach), ensure robust fallbacks.
  - Unignore uring tests in CI environments that support it; otherwise keep them gated.

- Accounting accuracy
  - Optionally include meta bytes in `disk_bytes_used`.
  - Track per-key sizes to better inform eviction and purges.

- Partial streaming and range support
  - Consider enabling limited seek on partial reads by reading from disk as needed once data has been written beyond the in-memory base.
  - Make chunk sizes for reads/writes configurable; add backpressure controls.

- Observability expansion
  - Add counters for finalize success/failure and error classes (open/write/fsync/rename).
  - Add histograms for finalize durations separately from write/read.
  - io_uring manager metrics: queue depth, open FD cache size, operation failures.
  - Lightweight tracing spans around disk ops and finalize.

- Operational safeguards
  - Add a periodic janitor to prune stale tmp dirs and apply retention policy if needed.
  - Provide a CLI or admin endpoint (in the example) for triggering reindex, purge-by-prefix, and metrics dump.

- Disk layout options
  - Keep current sharded layout by short hash prefix; optionally allow configurable fan-out and levels.
  - Document inode considerations and recommended filesystems.

## Future Directions

- Hot-cache layer for small objects
  - Optional in-memory layer for very small or very hot items to avoid disk hit path entirely.

- Tiered-cache integration
  - Design a secondary shared/distributed store (beyond MVP) and a smart admission/promote policy.

- Advanced eviction
  - Pluggable policies (LFU, size-aware, segmented LRU) and tighter integration to preempt capacity failures.

- Crash-recovery validation
  - Fault-injection tests around fsync/rename windows and kill -9 during writes, verifying that atomic publish guarantees hold and indexer cleanup works.

- Security and robustness
  - Bound key lengths, sanitize inputs for logging, consider quotas per namespace, and cap open files.

- Cross-platform
  - Provide a pure-Tokio build for non-Linux environments; document io_uring Linux requirements.

## Benchmarking Plan

- Goals
  - Validate latency (p50/p95/p99) and throughput for hits/misses.
  - Compare Tokio vs. io_uring read paths under varied loads.
  - Assess behavior under concurrency, capacity pressure, and partial-streaming scenarios.

- Workloads
  - Object sizes: 1 KiB, 100 KiB, 1 MiB, 10 MiB, 100 MiB.
  - Mix: 80/20 hits/misses; 5% range requests; 10% partial attaches.
  - Concurrency: 1, 8, 32, 128, 512 concurrent clients.
  - Page cache states: cold (drop_caches) vs. warm (pre-warmed).

- Scenarios
  - Memory hit (immediately after finish, before demotion) vs. disk hit (fresh process).
  - Partial streaming: one writer with N readers attaching mid-stream.
  - Capacity pressure: repeatedly write until `max_disk_bytes` is approached; measure finish latency and eviction behavior.
  - Failure injection: simulate write/fsync/rename errors; verify cleanup and counters.

- Metrics to capture
  - Application: cache hits/misses, stream attach ok/miss, finalize success/failure, read/write/finalize histograms, feature gauges.
  - System: CPU, RSS, IO utilization, io_uring queue depth (if enabled), page cache hit ratio (indirect via tools).

- Tooling
  - Load: `wrk`, `h2load`, or `vegeta` against the Pingora example.
  - Microbench: `criterion` for isolated handler paths (seek/read sizes, partial streaming loop).
  - System: `perf`, `iostat`, `blktrace`, `bcc/eBPF` tools if available.
  - Orchestrate with simple scripts; export Prometheus metrics to Grafana dashboards for runs.

- Reporting
  - Track latency histograms and throughput across configurations (Tokio vs. io_uring; atomic vs. non-atomic finalize).
  - Record CPU/IO utilization vs. RPS; document scaling curves and tail latencies.

## Acceptance Checks for Next Release

- All finalize paths either atomic or explicitly fsynced; tests for crash windows.
- Eviction-before-admission attempts implemented (or clearly documented as out-of-scope) with metrics.
- Startup indexer rebuilds state; tmp cleanup verified.
- io_uring feature-gated; tests run on supported CI.
- Benchmarks executed with published results and tuning guidance.
