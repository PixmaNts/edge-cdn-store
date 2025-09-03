# Benchmark Proposal (WSL2‑Friendly)

This document outlines a practical plan to benchmark `edge-cdn-store` on a single developer machine (e.g., WSL2) without specialized hardware. The plan targets cache correctness, latency, throughput, and persistence behavior under realistic but constrained conditions.

## Scope and Goals

- Compare miss vs hit latency and throughput.
- Validate persistence/reload-from-disk behavior across restarts.
- Stress streaming partial writes with concurrent readers.
- Observe CPU, memory, and disk I/O under different object sizes and hit ratios.

## Workload Setup

- Origin server (local, to avoid WAN variance):
  - Option A: `docker run --rm -p 18080:80 kennethreitz/httpbin` (or similar lightweight image).
  - Option B: a tiny local HTTP server exposing endpoints that:
    - Return configurable body sizes (1 KB, 100 KB, 5–20 MB).
    - Emit `Cache-Control` headers.
    - Optionally stream/chunk with configurable inter-chunk delay for partial-write tests.
- Proxy under test: `examples/cdn_server.rs` configured to point at the local origin.
  - Expose `/metrics` (Prometheus) at `127.0.0.1:6192`.
  - Make origin address, storage dir, and cache toggles configurable via env.

## Load Tools

- `wrk` / `wrk2` (Lua scripting for dynamic keys and Zipfian distributions).
- `vegeta` or `bombardier` (alternate HTTP load tools).
- `h2load` (optional, to assess H2 client path).
- System stat collectors: `pidstat`, `top/htop`, `iostat`, `vmstat`.
- Scrape Prometheus endpoint for counters/histograms.

## Key Scenarios

1) Cold miss baseline
   - Direct origin (no proxy), proxy with cache disabled, proxy with cache enabled (first request).
   - KPIs: p50/p90/p99 latency, RPS at fixed concurrency.

2) Hot hit baseline
   - Pre-warm a small key set; runs are steady-state hits.
   - Body sizes: 1 KB, 100 KB, 5 MB.
   - Concurrency: 1, 8, 64, 256.
   - KPIs: p50/p99 latency, RPS, CPU%, RSS, disk reads (should be near-zero if hot).

3) Mixed hit ratio (Zipfian)
   - 10k keys; skew s=1.0 (80/20) and s=0.6 (flatter).
   - Body sizes mixed: 1 KB (60%), 100 KB (30%), 5 MB (10%).
   - KPIs: tail latency p99, cache hit ratio, RPS.

4) Streaming partials
   - Origin streams 5–20 MB with 10–50 ms chunk delay.
   - Start N readers on the same key while writer is in progress.
   - Compare with/without `lookup_streaming_write` tag matching.
   - KPIs: reader first-byte latency, sustained throughput, CPU.

5) Persistence reload
   - Warm cache, stop proxy, restart with same data dir.
   - Measure first lookup (disk reload) and subsequent hot hits.
   - KPIs: disk-read latency p50/p99, RPS, page cache effects.

6) Disk stress
   - Large bodies (50–200 MB) with unique keys to exceed RAM and hit disk paths.
   - KPIs: disk throughput, iops, page cache behavior, tail latencies.

## Methodology and Hygiene

- Warm-up (10–30 s) before measuring steady-state.
- Fixed-duration measurements (60–120 s) per scenario; repeat 3× and report median.
- Control variables:
  - WSL2 CPU/RAM limits (document settings).
  - Localhost paths (no WAN/DNS); keep keepalive on.
  - Reuse the same concurrency profiles across runs.
- Logging/metrics capture per run:
  - Load tool results (latency percentiles, RPS).
  - Prometheus counters deltas (hits, misses, errors; later add disk ops and latency histograms).
  - `pidstat` (CPU/mem), `iostat` (disk), optional `vmstat`.

## Data to Collect (per run)

- Tool output: p50/p90/p99 latency, RPS.
- Prometheus deltas: cache hits/misses, request totals; later add disk ops and durations.
- System: CPU% (user/sys), RSS, iops/throughput.
- Config: concurrency, body size(s), key distribution, cache toggles.

## Hit Ratio and Key Generation

- `wrk` Lua to build request URIs like `/obj/<size>/<idx>`.
- Zipfian selection for `idx` to simulate skew; deterministic seed for repeatability.
- Streaming: `/stream/<size>` with chunk delay query parameter if available.

## Step‑by‑Step Execution Plan

1) Stand up local origin; validate fixed and streamed responses.
2) Point `cdn_server` at local origin; confirm `/metrics` is scraping.
3) Prepare `wrk` scripts:
   - Fixed key (hot-hit), mixed-size keys, Zipfian keys.
4) Execute scenarios 1→6:
   - Warm-up, measure, capture metrics and system stats.
   - Repeat 3×, store outputs with timestamps.
5) Summarize results:
   - Table of scenario × (p50/p99, RPS, CPU, RSS), brief notes on observed bottlenecks.

## Success Criteria (initial)

- Hot-hit p99 latency: sub-ms for 1 KB, <5 ms for ~100 KB.
- Hit RPS scales with concurrency until CPU saturates; disk reads negligible when hot.
- Streaming readers see progressive data with stable throughput.
- Reload-from-disk: p99 first-hit acceptable (e.g., <50 ms for ~100 KB).

## Future Enhancements

- Add `io_uring` backend flag and repeat disk-heavy scenarios.
- Implement atomic publish (write `*.part`, fsync file + parent dir, rename) and measure write path latency.
- Add Prometheus counters/gauges/histograms for hits/misses, disk ops, write/read durations; track percentiles.
- Consider HTTPS/H2 upstream and an optional warm-up pass in the example to reduce first-miss latency.

