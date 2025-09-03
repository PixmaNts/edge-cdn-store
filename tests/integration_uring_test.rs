//! Integration tests with io_uring enabled to validate parity with tokio backend.

mod uring_integration {
    use bytes::Bytes;
    use edge_cdn_store::{EdgeMemoryStorage, EdgeMemoryStorageBuilder};
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
        p.push(format!("edge_store_integ_uring_{label}_{ts}"));
        p
    }

    fn make_key(path: &str) -> CacheKey {
        CacheKey::new("ns", path, "u1")
    }

    fn make_meta(max_age_secs: u64) -> CacheMeta {
        let created = SystemTime::now();
        let fresh_until = created + Duration::from_secs(max_age_secs);
        let header = ResponseHeader::build(200, None).unwrap();
        CacheMeta::new(fresh_until, created, 0, 0, header)
    }

    #[tokio::test]
    async fn write_hit_reload_and_purge_uring() -> Result<()> {
        let root = temp_root("persist");
        let storage1: &'static EdgeMemoryStorage =
            Box::leak(Box::new(EdgeMemoryStorage::with_disk_root(root.clone())));
        let trace = Span::inactive().handle();
        let key = make_key("/resource");
        let meta = make_meta(60);

        // write and finish using default (tokio write) path
        let mut mh = storage1.get_miss_handler(&key, &meta, &trace).await?;
        mh.write_body(Bytes::from_static(b"hello"), true).await?;
        mh.finish().await?;

        // new instance reloads from disk using io_uring read path
        let storage2: &'static EdgeMemoryStorage = Box::leak(Box::new(
            EdgeMemoryStorageBuilder::new()
                .with_disk_root(root)
                .with_io_uring_enabled(Some(true))
                .build(),
        ));
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
        assert!(hit.is_some(), "io_uring disk lookup should hit");

        // purge via io_uring-enabled storage
        let ckey = key.to_compact();
        let _ = storage2
            .purge(&ckey, PurgeType::Invalidation, &trace)
            .await?;
        assert!(storage2.lookup(&key, &trace).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn disk_hit_handler_reads_and_seek_uring() -> Result<()> {
        // Write with one instance, then read from a fresh io_uring-enabled instance
        let root = temp_root("disk_seek");
        let storage1: &'static EdgeMemoryStorage =
            Box::leak(Box::new(EdgeMemoryStorage::with_disk_root(root.clone())));
        let trace = Span::inactive().handle();
        let key = make_key("/disk_seek");
        let meta = make_meta(60);

        let mut mh = storage1.get_miss_handler(&key, &meta, &trace).await?;
        mh.write_body(Bytes::from_static(b"hello world"), true).await?;
        mh.finish().await?;

        // New storage instance uses io_uring-backed handler
        let storage2: &'static EdgeMemoryStorage = Box::leak(Box::new(
            EdgeMemoryStorageBuilder::new()
                .with_disk_root(root)
                .with_io_uring_enabled(Some(true))
                .build(),
        ));

        // Retry until background persist has completed
        let (_meta2, mut hit) = {
            let mut tries = 0;
            loop {
                if let Some(h) = storage2.lookup(&key, &trace).await? {
                    break h;
                }
                tries += 1;
                if tries > 50 {
                    panic!("disk files not visible for io_uring lookup");
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

        // Get a new handler and verify seek works on uring handler too
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

    #[tokio::test]
    async fn streaming_partial_read_uring_mode() -> Result<()> {
        // Enable io_uring globally for the storage; partial reads should still work the same
        let root = temp_root("streaming");
        let storage: &'static EdgeMemoryStorage = Box::leak(Box::new(
            EdgeMemoryStorageBuilder::new()
                .with_disk_root(root)
                .with_io_uring_enabled(Some(true))
                .build(),
        ));
        let key = make_key("/partial");
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
}

