use bytes::Bytes;
use edge_cdn_store::{EdgeMemoryStorage, EdgeStoreConfig};
use pingora::cache::Storage;
use pingora::cache::trace::Span;
use pingora::cache::{CacheKey, CacheMeta};
use pingora::http::ResponseHeader;
use std::time::{Duration, SystemTime};

fn make_meta() -> CacheMeta {
    let created = SystemTime::now();
    let header = ResponseHeader::build(200, None).unwrap();
    CacheMeta::new(created + Duration::from_secs(60), created, 0, 0, header)
}

#[tokio::test]
async fn max_disk_bytes_blocks_finish() {
    let cfg = EdgeStoreConfig {
        max_disk_bytes: Some(5),
        ..Default::default()
    };
    let storage: &'static EdgeMemoryStorage =
        Box::leak(Box::new(EdgeMemoryStorage::from_config(&cfg)));
    let trace = Span::inactive().handle();
    let key = CacheKey::new("ns", "/disk1", "u");
    let meta = make_meta();
    let mut mh = storage.get_miss_handler(&key, &meta, &trace).await.unwrap();
    mh.write_body(Bytes::from_static(b"123456"), true)
        .await
        .unwrap();
    let res = mh.finish().await;
    assert!(res.is_err());
}

#[tokio::test]
async fn max_disk_bytes_allows_under_limit() {
    let cfg = EdgeStoreConfig {
        max_disk_bytes: Some(6),
        ..Default::default()
    };
    let storage: &'static EdgeMemoryStorage =
        Box::leak(Box::new(EdgeMemoryStorage::from_config(&cfg)));
    let trace = Span::inactive().handle();
    let key = CacheKey::new("ns", "/disk2", "u");
    let meta = make_meta();
    let mut mh = storage.get_miss_handler(&key, &meta, &trace).await.unwrap();
    mh.write_body(Bytes::from_static(b"123456"), true)
        .await
        .unwrap();
    let res = mh.finish().await;
    assert!(res.is_ok());
}

#[tokio::test]
async fn max_disk_bytes_blocks_second_admission() {
    let cfg = EdgeStoreConfig {
        max_disk_bytes: Some(6),
        ..Default::default()
    };
    let storage: &'static EdgeMemoryStorage =
        Box::leak(Box::new(EdgeMemoryStorage::from_config(&cfg)));
    let trace = Span::inactive().handle();
    let meta = make_meta();

    let key1 = CacheKey::new("ns", "/disk3a", "u");
    let mut mh1 = storage
        .get_miss_handler(&key1, &meta, &trace)
        .await
        .unwrap();
    mh1.write_body(Bytes::from_static(b"123456"), true)
        .await
        .unwrap();
    assert!(mh1.finish().await.is_ok());

    let key2 = CacheKey::new("ns", "/disk3b", "u");
    let mut mh2 = storage
        .get_miss_handler(&key2, &meta, &trace)
        .await
        .unwrap();
    mh2.write_body(Bytes::from_static(b"1"), true)
        .await
        .unwrap();
    assert!(mh2.finish().await.is_err());
}

#[tokio::test]
async fn max_object_bytes_blocks_large_single_write() {
    let cfg = EdgeStoreConfig {
        max_object_bytes: Some(5),
        ..Default::default()
    };
    let storage: &'static EdgeMemoryStorage =
        Box::leak(Box::new(EdgeMemoryStorage::from_config(&cfg)));
    let trace = Span::inactive().handle();
    let key = CacheKey::new("ns", "/cap1", "u");
    let meta = make_meta();
    let mut mh = storage.get_miss_handler(&key, &meta, &trace).await.unwrap();
    // 6 bytes should exceed 5
    let err = mh
        .write_body(Bytes::from_static(b"123456"), true)
        .await
        .unwrap_err();
    let s = format!("{err}");
    assert!(s.contains("ObjectTooLarge"));
}

#[tokio::test]
async fn max_object_bytes_blocks_on_second_chunk() {
    let cfg = EdgeStoreConfig {
        max_object_bytes: Some(5),
        ..Default::default()
    };
    let storage: &'static EdgeMemoryStorage =
        Box::leak(Box::new(EdgeMemoryStorage::from_config(&cfg)));
    let trace = Span::inactive().handle();
    let key = CacheKey::new("ns", "/cap2", "u");
    let meta = make_meta();
    let mut mh = storage.get_miss_handler(&key, &meta, &trace).await.unwrap();
    mh.write_body(Bytes::from_static(b"1234"), false)
        .await
        .unwrap();
    // Next 2 bytes push to 6 > 5
    let err = mh
        .write_body(Bytes::from_static(b"12"), true)
        .await
        .unwrap_err();
    let s = format!("{err}");
    assert!(s.contains("ObjectTooLarge"));
}

#[tokio::test]
async fn max_object_bytes_allows_under_limit() {
    let cfg = EdgeStoreConfig {
        max_object_bytes: Some(6),
        ..Default::default()
    };
    let storage: &'static EdgeMemoryStorage =
        Box::leak(Box::new(EdgeMemoryStorage::from_config(&cfg)));
    let trace = Span::inactive().handle();
    let key = CacheKey::new("ns", "/cap3", "u");
    let meta = make_meta();
    let mut mh = storage.get_miss_handler(&key, &meta, &trace).await.unwrap();
    mh.write_body(Bytes::from_static(b"123456"), true)
        .await
        .unwrap();
    let _ = mh.finish().await.unwrap();
}

#[tokio::test]
async fn max_partial_writes_blocks_second_writer() {
    let cfg = EdgeStoreConfig {
        max_partial_writes: Some(1),
        ..Default::default()
    };
    let storage: &'static EdgeMemoryStorage =
        Box::leak(Box::new(EdgeMemoryStorage::from_config(&cfg)));
    let trace = Span::inactive().handle();
    let key1 = CacheKey::new("ns", "/p1", "u");
    let key2 = CacheKey::new("ns", "/p2", "u");
    let meta = make_meta();

    let _mh1 = storage
        .get_miss_handler(&key1, &meta, &trace)
        .await
        .unwrap();
    let res = storage.get_miss_handler(&key2, &meta, &trace).await;
    assert!(res.is_err());
}
