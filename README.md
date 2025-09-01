# edge-cdn-store

A CDN cache storage backend implementation for the `pingora_cache` library, developed as part of a Wasmer technical evaluation.

## Overview

This project implements a storage layer for CDN caching that integrates with Cloudflare's `pingora_cache` framework. The implementation focuses on high-concurrency scenarios, low memory usage, and pluggable observability features.

## Features

- High-performance concurrent metadata operations
- Tokio async runtime integration
- Memory-efficient design for scale
- Pluggable observability (metrics, logging, health checks)
- Integration with pingora_cache EvictionManager
- Configurable cache behavior
- Designed for tiered cache architecture

## Requirements

- Rust 2024 edition
- Tokio runtime
- pingora-cache library

## Architecture

The implementation provides a custom storage backend for `pingora_cache` by implementing the `Storage` trait. See `design/` directory for detailed proposal and requirements.
