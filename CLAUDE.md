# Claude Instructions for edge-cdn-store

## Project Overview
This is a Wasmer technical evaluation project implementing a CDN cache storage backend for the `pingora_cache` library.

## Project Structure
- `design/` - Contains requirements and proposal documents
- `src/` - Rust source code implementing the storage backend
- Target: Implement `pingora_cache::Storage` trait

## Key Requirements
- High concurrency support for metadata lookups/inserts/retrievals
- Tokio runtime integration
- Low memory usage
- Pluggable observability (Prometheus metrics, logging, health checks)
- Integration with pingora_cache EvictionManager
- Design for tiered cache storage (local + distributed)
- Configurable behavior

## Development Commands
- `cargo build` - Build the project
- `cargo test` - Run tests
- `cargo clippy` - Run linter
- `cargo fmt` - Format code

## Dependencies
- pingora = "0.6.0" (with cache feature)
- tokio = "1.47.1" (full features)

## Reference Materials
- pingora_cache docs: https://docs.rs/pingora-cache/latest/pingora_cache/
- pingora repo: Local checkout in ../pingora/
- Focus on pingora-cache crate for Storage trait implementation

## Deliverables
1. Design proposal (design/proposal.md)
2. Prototype implementation of Storage trait