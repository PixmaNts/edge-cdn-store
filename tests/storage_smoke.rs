use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use edge_cdn_store::EdgeMemoryStorage;
use pingora::cache::storage::Storage;
use pingora::cache::trace::Span;
use pingora::cache::{CacheKey, CacheMeta};
use pingora::http::ResponseHeader;
use pingora::prelude::Result;

fn temp_root(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("edge_store_test_{}_{}", label, ts));
    p
}

fn make_key() -> CacheKey {
    CacheKey::new("ns", "/resource", "u1")
}

fn make_meta(max_age_secs: u64) -> CacheMeta {
    let created = SystemTime::now();
    let fresh_until = created + Duration::from_secs(max_age_secs);
    let header = ResponseHeader::build(200, None).unwrap();
    CacheMeta::new(fresh_until, created, 0, 0, header)
}

#[tokio::test]
async fn miss_to_hit_and_disk_reload() -> Result<()> {
    let root = temp_root("miss_to_hit");
    let storage: &'static EdgeMemoryStorage = Box::leak(Box::new(EdgeMemoryStorage::with_disk_root(root.clone())));

    let key = make_key();
    let meta = make_meta(60);

    // Miss handler and write body
    let trace = Span::inactive().handle();
    let mut mh = storage.get_miss_handler(&key, &meta, &trace).await?;
    mh.write_body(Bytes::from_static(b"hello world"), true).await?;
    mh.finish().await?;

    // Lookup should hit memory
    let (_meta2, mut hit) = storage.lookup(&key, &trace).await?.expect("hit");
    let mut body = Vec::new();
    while let Some(chunk) = hit.read_body().await? { body.extend_from_slice(chunk.as_ref()); }
    assert_eq!(body, b"hello world");

    // New storage pointing to same disk root should load from disk (wait until persisted)
    let storage2: &'static EdgeMemoryStorage = Box::leak(Box::new(EdgeMemoryStorage::with_disk_root(root)));
    let (_meta3, mut hit2) = loop {
        if let Some(hit) = storage2.lookup(&key, &trace).await? { break hit; }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let mut body2 = Vec::new();
    while let Some(chunk) = hit2.read_body().await? { body2.extend_from_slice(chunk.as_ref()); }
    assert_eq!(body2, b"hello world");

    Ok(())
}

#[tokio::test]
async fn streaming_partial_read() -> Result<()> {
    let root = temp_root("streaming");
    let storage: &'static EdgeMemoryStorage = Box::leak(Box::new(EdgeMemoryStorage::with_disk_root(root)));
    let key = make_key();
    let meta = make_meta(60);

    let trace = Span::inactive().handle();
    let mut mh = storage
        .get_miss_handler(&key, &meta, &trace)
        .await?;

    // Reader task attaches to the same streaming write via tag
    let tag = mh.streaming_write_tag().unwrap().to_vec();
    let storage_c = storage;
    let key_c = key.clone();
    let reader: tokio::task::JoinHandle<Result<Vec<u8>>> = tokio::spawn(async move {
        let trace = Span::inactive().handle();
        let (_m2, mut hit) = storage_c
            .lookup_streaming_write(&key_c, Some(&tag[..]), &trace)
            .await?
            .expect("partial hit");
        let mut read = Vec::new();
        // Read first chunk
        if let Some(chunk) = hit.read_body().await? { read.extend_from_slice(chunk.as_ref()); }
        // Read second chunk and EOF
        while let Some(chunk) = hit.read_body().await? { read.extend_from_slice(chunk.as_ref()); }
        Result::Ok(read)
    });

    // Writer sends two chunks and finishes
    mh.write_body(Bytes::from_static(b"hello "), false).await?;
    // small delay to simulate streaming
    tokio::time::sleep(Duration::from_millis(10)).await;
    mh.write_body(Bytes::from_static(b"world"), true).await?;
    mh.finish().await?;

    let collected = reader.await.unwrap()?;
    assert_eq!(collected, b"hello world");
    Ok(())
}
//! Storage integration smoke tests.
//!
//! Goals:
//! - Verify write → lookup (memory hit) → reload from disk across a new instance.
//! - Ensure streaming partial reads deliver chunks as they become available.
//!
//! Notes:
//! - Tracing: tests use `Span::inactive()` to satisfy the API without a tracing backend.
//! - Lifetime: storage is `Box::leak`ed to get a `'static` reference required by the trait.
//! - Persistence: disk writes happen in a background task; the test loops until the object appears on disk.
