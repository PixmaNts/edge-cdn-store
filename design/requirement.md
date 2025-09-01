# Implement a CDN cache storage backend for the `pingora_cache` library

## Background

Wasmer Edge is adding CDN caching support, similar to Cloudflare and other CDNs. That means Edge will cache HTTP responses from customer apps, and reuse cached responses when appropriate, as determined by app-specific configuration, HTTP response headers, and other heuristics.

Your challenge is to implement a small piece of this functionality.

Our CDN implementation will be based on `pingora_cache` (<https://docs.rs/pingora-cache/latest/pingora_cache/>), a Rust crate that integrates with the `pingora` proxy framework by Cloudflare (<https://github.com/cloudflare/pingora>).

pingora_cache uses a pluggable storage mechanism.

## Requirements

The storage layer for the cache must:

* Efficiently support a large amount of concurrent activity (metadata lookups, inserts, and retrievals)
* Integrate well into the context of a Tokio runtime
* Keep memory usage low to enable servers to scale to a large number of requests
* Provide pluggable observability that enables introspection into the performance and activity of the cache (through Prometheus metrics, logging, external health checks, …)
* Integrate well with the pingora_cache EvictionManager to support keeping the local cache size in check
* Account for tiered cache storage, with a primary local cache and an additional shared or distributed cache layer that allows caches to be shared across servers
(implementing a shared store is out of scope for this task, but the design must account for this functionality and be prepared for integration)
* Provide configuration to tune the behavior of the cache  

## Deliverables

1. Design Proposal & Specification
    Write a proposal that evaluates the design space and specifies the proposed implementation.
    The main goal of the proposal is to provide a rough sketch of the implementation, including how it will fulfill the requirements.
    *Please use the template attached below.*  

2. Prototype implementation
    A standalone Rust crate, edge-cdn-store, that provides an implementation for the pingora_cache::Storage trait.
    This implementation is not expected to be production quality, but should serve as a demonstrator and provide a solid baseline that can be polished.
    If you are the right fit for Wasmer, your first task will most likely be to integrate your solution into the product!  
