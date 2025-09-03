//! Integration tests for edge-cdn-store storage implementation
//!
//! Organized by feature area: basics, persistence, and streaming.

mod basics {
    use edge_cdn_store::EdgeMemoryStorage;
    use std::sync::Arc;

    #[tokio::test]
    async fn storage_constructs() {
        let _storage = EdgeMemoryStorage::new();
    }

    #[tokio::test]
    async fn concurrent_access_does_not_panic() {
        let storage = Arc::new(EdgeMemoryStorage::new());
        let handles: Vec<_> = (0..10)
            .map(|_| {
                let s = Arc::clone(&storage);
                tokio::spawn(async move {
                    let _ = &s.write_counter;
                })
            })
            .collect();
        for h in handles {
            h.await.unwrap();
        }
    }

    #[test]
    fn dashmap_readers_share_arc() {
        use dashmap::DashMap;
        use std::sync::Arc;
        let cache: DashMap<String, Arc<Vec<u8>>> = DashMap::new();
        let key = "k";
        let data = Arc::new(b"v".to_vec());
        cache.insert(key.to_string(), data.clone());
        assert_eq!(cache.get(key).unwrap().value().as_slice(), b"v");
        assert_eq!(cache.get(key).unwrap().value().as_slice(), b"v");
    }
}

mod persistence_and_streaming {
    use bytes::Bytes;
    use edge_cdn_store::EdgeMemoryStorage;
    use pingora::cache::storage::{PurgeType, Storage};
    use pingora::cache::trace::Span;
    use pingora::cache::{CacheKey, CacheMeta};
    use pingora::http::ResponseHeader;
    use pingora::prelude::Result;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn temp_root(label: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("edge_store_integ_{label}_{ts}"));
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
    async fn write_hit_reload_and_purge() -> Result<()> {
        let root = temp_root("persist");
        let storage: &'static EdgeMemoryStorage =
            Box::leak(Box::new(EdgeMemoryStorage::with_disk_root(root.clone())));
        let trace = Span::inactive().handle();
        let key = make_key();
        let meta = make_meta(60);

        // write and finish
        let mut mh = storage.get_miss_handler(&key, &meta, &trace).await?;
        mh.write_body(Bytes::from_static(b"hello"), true).await?;
        mh.finish().await?;

        // memory hit
        assert!(storage.lookup(&key, &trace).await?.is_some());

        // new instance reloads (wait briefly until background persist is visible)
        let storage2: &'static EdgeMemoryStorage =
            Box::leak(Box::new(EdgeMemoryStorage::with_disk_root(root)));
        let mut tries = 0;
        let hit = loop {
            if let Some(h) = storage2.lookup(&key, &trace).await? {
                break Some(h);
            }
            tries += 1;
            if tries > 50 {
                break None;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(hit.is_some());

        // purge
        let ckey = key.to_compact();
        let _ = storage2
            .purge(&ckey, PurgeType::Invalidation, &trace)
            .await?;
        assert!(storage2.lookup(&key, &trace).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn streaming_partial_read() -> Result<()> {
        let root = temp_root("streaming");
        let storage: &'static EdgeMemoryStorage =
            Box::leak(Box::new(EdgeMemoryStorage::with_disk_root(root)));
        let key = CacheKey::new("ns", "/partial", "u1");
        let meta = make_meta(60);

        let trace = Span::inactive().handle();
        let mut mh = storage.get_miss_handler(&key, &meta, &trace).await?;

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
            if let Some(chunk) = hit.read_body().await? {
                read.extend_from_slice(chunk.as_ref());
            }
            while let Some(chunk) = hit.read_body().await? {
                read.extend_from_slice(chunk.as_ref());
            }
            Result::Ok(read)
        });

        mh.write_body(Bytes::from_static(b"hello "), false).await?;
        tokio::time::sleep(Duration::from_millis(10)).await;
        mh.write_body(Bytes::from_static(b"world"), true).await?;
        mh.finish().await?;

        let collected = reader.await.unwrap()?;
        assert_eq!(collected, b"hello world");
        Ok(())
    }

    #[tokio::test]
    async fn lookup_streaming_write_mismatched_tag_returns_none() -> Result<()> {
        let root = temp_root("stream_tag_mismatch");
        let storage: &'static EdgeMemoryStorage =
            Box::leak(Box::new(EdgeMemoryStorage::with_disk_root(root)));
        let trace = Span::inactive().handle();
        let key = CacheKey::new("ns", "/partial", "u1");
        let meta = make_meta(60);

        let mut mh = storage.get_miss_handler(&key, &meta, &trace).await?;
        let mut wrong = mh.streaming_write_tag().unwrap().to_vec();
        wrong[0] ^= 0xff;

        let attach = storage
            .lookup_streaming_write(&key, Some(&wrong[..]), &trace)
            .await?;
        assert!(attach.is_none());

        mh.write_body(Bytes::from_static(b"ok"), true).await?;
        let _ = mh.finish().await?;
        Ok(())
    }

    #[tokio::test]
    async fn update_meta_persists_to_disk() -> Result<()> {
        use std::fs as stdfs;
        let root = temp_root("update_meta");
        let storage: &'static EdgeMemoryStorage =
            Box::leak(Box::new(EdgeMemoryStorage::with_disk_root(root.clone())));
        let trace = Span::inactive().handle();

        let key = CacheKey::new("ns", "/update", "u1");
        let meta = make_meta(60);
        let mut mh = storage.get_miss_handler(&key, &meta, &trace).await?;
        mh.write_body(Bytes::from_static(b"hello world"), true)
            .await?;
        mh.finish().await?;

        let mut new_header = ResponseHeader::build(200, None).unwrap();
        new_header.insert_header("x-test", "1").unwrap();
        let new_meta = CacheMeta::new(
            SystemTime::now() + Duration::from_secs(120),
            SystemTime::now(),
            0,
            0,
            new_header,
        );
        let _ = storage.update_meta(&key, &new_meta, &trace).await?;

        let mut tries = 0;
        loop {
            let mut found_internal = false;
            let mut found_header = false;
            let mut found_body = false;
            let mut stack = vec![root.clone()];
            while let Some(path) = stack.pop() {
                if let Ok(rd) = stdfs::read_dir(&path) {
                    for entry in rd.flatten() {
                        let p = entry.path();
                        if p.is_dir() {
                            stack.push(p);
                        } else if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                            match name {
                                "meta_internal.bin" => found_internal = true,
                                "meta_header.bin" => found_header = true,
                                "body.bin" => found_body = true,
                                _ => {}
                            }
                        }
                    }
                }
            }
            if found_internal && found_header && found_body {
                break;
            }
            tries += 1;
            if tries > 50 {
                panic!("meta files not visible on disk");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(())
    }

    #[tokio::test]
    async fn complete_hit_handler_supports_seek() -> Result<()> {
        let root = temp_root("seek");
        let storage: &'static EdgeMemoryStorage =
            Box::leak(Box::new(EdgeMemoryStorage::with_disk_root(root)));
        let trace = Span::inactive().handle();
        let key = CacheKey::new("ns", "/seek", "u1");
        let meta = make_meta(60);

        let mut mh = storage.get_miss_handler(&key, &meta, &trace).await?;
        mh.write_body(Bytes::from_static(b"hello world"), true)
            .await?;
        mh.finish().await?;

        let (_meta2, mut hit) = storage.lookup(&key, &trace).await?.expect("hit");
        assert!(hit.can_seek());
        hit.seek(6, None)?;
        let mut buf = Vec::new();
        while let Some(chunk) = hit.read_body().await? {
            buf.extend_from_slice(chunk.as_ref());
        }
        assert_eq!(buf, b"world");
        Ok(())
    }

    #[tokio::test]
    async fn disk_hit_handler_reads_and_seek() -> Result<()> {
        // Write with one instance, then read from a fresh instance (forces disk path)
        let root = temp_root("disk_seek");
        let storage1: &'static EdgeMemoryStorage =
            Box::leak(Box::new(EdgeMemoryStorage::with_disk_root(root.clone())));
        let trace = Span::inactive().handle();
        let key = CacheKey::new("ns", "/disk_seek", "u1");
        let meta = make_meta(60);

        let mut mh = storage1.get_miss_handler(&key, &meta, &trace).await?;
        mh.write_body(Bytes::from_static(b"hello world"), true).await?;
        mh.finish().await?;

        // New storage instance uses disk-backed handler
        let storage2: &'static EdgeMemoryStorage =
            Box::leak(Box::new(EdgeMemoryStorage::with_disk_root(root)));

        // Retry until background persist has completed
        let (_meta2, mut hit) = {
            let mut tries = 0;
            loop {
                if let Some(h) = storage2.lookup(&key, &trace).await? {
                    break h;
                }
                tries += 1;
                if tries > 50 {
                    panic!("disk files not visible for lookup");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };

        // Read full body
        let mut buf = Vec::new();
        while let Some(chunk) = hit.read_body().await? {
            buf.extend_from_slice(chunk.as_ref());
        }
        assert_eq!(buf, b"hello world");

        // Get a new handler and verify seek works on disk handler too
        let (_m, mut hit2) = storage2.lookup(&key, &trace).await?.expect("hit again");
        assert!(hit2.can_seek());
        hit2.seek(6, None)?;
        let mut buf2 = Vec::new();
        while let Some(chunk) = hit2.read_body().await? {
            buf2.extend_from_slice(chunk.as_ref());
        }
        assert_eq!(buf2, b"world");
        Ok(())
    }
}
