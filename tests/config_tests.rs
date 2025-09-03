use edge_cdn_store::{EdgeMemoryStorage, EdgeMemoryStorageBuilder, EdgeStoreConfig};
use std::path::PathBuf;

fn res(p: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/resources").join(p)
}

#[test]
fn parse_full_yaml() {
    let cfg = EdgeStoreConfig::from_yaml_file(res("full.yaml"));
    assert_eq!(cfg.disk_root.as_deref(), Some("/tmp/edge_store_full"));
    assert_eq!(cfg.max_disk_bytes, Some(1073741824));
    assert_eq!(cfg.max_object_bytes, Some(104857600));
    assert_eq!(cfg.max_partial_writes, Some(64));
    assert_eq!(cfg.atomic_publish, Some(true));
    assert_eq!(cfg.io_uring_enabled, Some(false));
}

#[test]
fn parse_partial_yaml() {
    let cfg = EdgeStoreConfig::from_yaml_file(res("partial.yaml"));
    assert_eq!(cfg.disk_root.as_deref(), Some("/tmp/edge_store_partial"));
    assert!(cfg.max_disk_bytes.is_none());
    assert!(cfg.max_object_bytes.is_none());
}

#[test]
fn parse_unknown_keys_yaml() {
    // unknown keys should be ignored without error
    let cfg = EdgeStoreConfig::from_yaml_file(res("unknown.yaml"));
    assert_eq!(cfg.disk_root.as_deref(), Some("/tmp/edge_store_unknown"));
}

#[test]
fn builder_builds_storage() {
    let storage = EdgeMemoryStorageBuilder::new()
        .with_disk_root("/tmp/edge_store_builder")
        .build();
    // ensure the constructed storage uses the provided root
    assert_eq!(storage.disk_root, PathBuf::from("/tmp/edge_store_builder"));
    let _ = storage; // suppress unused warning
}

#[test]
fn from_config_builds_storage() {
    let cfg = EdgeStoreConfig::from_yaml_file(res("partial.yaml"));
    let storage = EdgeMemoryStorage::from_config(&cfg);
    assert_eq!(storage.disk_root, PathBuf::from("/tmp/edge_store_partial"));
    let _ = storage;
}
