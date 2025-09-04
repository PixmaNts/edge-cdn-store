use crate::EdgeMemoryStorage;
use serde::Deserialize;
use std::path::PathBuf;

/// Configuration for EdgeMemoryStorage parsed from a Pingora YAML file.
///
/// Keys are top-level and prefixed with `edge-cdn-cache-...` to avoid collisions.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct EdgeStoreConfig {
    /// Root directory for on-disk data
    #[serde(rename = "edge-cdn-cache-disk-root")]
    pub disk_root: Option<String>,

    /// Maximum total disk usage in bytes (optional; enforced approximately on finish, body bytes only)
    #[serde(rename = "edge-cdn-cache-max-disk-bytes")]
    pub max_disk_bytes: Option<usize>,

    /// Maximum single object size admitted in bytes (optional; enforced during writes and on finish)
    #[serde(rename = "edge-cdn-cache-max-object-bytes")]
    pub max_object_bytes: Option<usize>,

    /// Maximum number of concurrent partial writes (optional; enforced on admission)
    #[serde(rename = "edge-cdn-cache-max-partial-writes")]
    pub max_partial_writes: Option<usize>,

    /// Enable atomic publish (write tmp + fsync + atomic rename). Recommended for stronger durability.
    #[serde(rename = "edge-cdn-cache-atomic-publish")]
    pub atomic_publish: Option<bool>,

    /// Enable io_uring backend when available (experimental): used for reads and streaming writes with fallback.
    #[serde(rename = "edge-cdn-cache-io-uring-enabled")]
    pub io_uring_enabled: Option<bool>,

    /// Max bytes to retain in memory for in-progress writes per object (to prevent OOM)
    #[serde(rename = "edge-cdn-cache-max-in-mem-partial-bytes")]
    pub max_in_mem_partial_bytes: Option<usize>,
}

impl EdgeStoreConfig {
    /// Parse configuration from a YAML string. Unknown keys are ignored.
    pub fn from_yaml_str(yaml: &str) -> Self {
        serde_yaml::from_str::<EdgeStoreConfig>(yaml).unwrap_or_default()
    }

    /// Read a YAML file from disk and parse the configuration.
    pub fn from_yaml_file(path: impl AsRef<std::path::Path>) -> Self {
        std::fs::read_to_string(&path)
            .ok()
            .as_deref()
            .map(Self::from_yaml_str)
            .unwrap_or_default()
    }

    /// Resolve disk root with environment fallback.
    pub fn resolve_disk_root(&self) -> PathBuf {
        if let Some(root) = &self.disk_root {
            return PathBuf::from(root);
        }
        if let Ok(env_root) = std::env::var("EDGE_STORE_DIR") {
            return PathBuf::from(env_root);
        }
        PathBuf::from("edge_store_data")
    }
}

/// Builder pattern for EdgeMemoryStorage configuration.
#[derive(Debug, Default, Clone)]
pub struct EdgeMemoryStorageBuilder {
    pub disk_root: PathBuf,
    pub max_disk_bytes: Option<usize>,
    pub max_object_bytes: Option<usize>,
    pub max_partial_writes: Option<usize>,
    pub atomic_publish: Option<bool>,
    pub io_uring_enabled: Option<bool>,
    pub max_in_mem_partial_bytes: Option<usize>,
}

impl EdgeMemoryStorageBuilder {
    pub fn new() -> Self {
        Self {
            disk_root: PathBuf::from("edge_store_data"),
            ..Default::default()
        }
    }
    pub fn with_disk_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.disk_root = root.into();
        self
    }
    pub fn with_max_disk_bytes(mut self, v: Option<usize>) -> Self {
        self.max_disk_bytes = v;
        self
    }
    pub fn with_max_object_bytes(mut self, v: Option<usize>) -> Self {
        self.max_object_bytes = v;
        self
    }
    pub fn with_max_partial_writes(mut self, v: Option<usize>) -> Self {
        self.max_partial_writes = v;
        self
    }
    pub fn with_atomic_publish(mut self, v: Option<bool>) -> Self {
        self.atomic_publish = v;
        self
    }
    pub fn with_io_uring_enabled(mut self, v: Option<bool>) -> Self {
        self.io_uring_enabled = v;
        self
    }
    pub fn with_max_in_mem_partial_bytes(mut self, v: Option<usize>) -> Self {
        self.max_in_mem_partial_bytes = v;
        self
    }

    /// Build an `EdgeMemoryStorage` instance from this builder.
    ///
    /// Currently only `disk_root` is enforced. Other options are accepted and will
    /// be applied as features are implemented (max sizes, atomic publish, io_uring, etc.).
    pub fn build(self) -> EdgeMemoryStorage {
        let mut s = EdgeMemoryStorage::with_disk_root(self.disk_root);
        // Apply capacity limits that the storage currently understands.
        s.max_object_bytes = self.max_object_bytes;
        s.max_partial_writes = self.max_partial_writes;
        s.max_disk_bytes = self.max_disk_bytes;
        // Apply toggles where implemented
        s.atomic_publish = self.atomic_publish.unwrap_or(true);
        s.io_uring_enabled = self.io_uring_enabled.unwrap_or(false);
        s.max_in_mem_partial_bytes = self.max_in_mem_partial_bytes;
        s.update_feature_gauges();
        s
    }
}
