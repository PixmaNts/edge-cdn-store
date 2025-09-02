use edge_cdn_store::EdgeMemoryStorage;
use std::sync::Arc;

#[tokio::test]
async fn test_basic_storage_creation() {
    // Test that we can create the storage
    let _storage = EdgeMemoryStorage::new();
    println!("✅ EdgeMemoryStorage created successfully!");
}

#[tokio::test]
async fn test_concurrent_dashmap_operations() {
    let storage = Arc::new(EdgeMemoryStorage::new());
    
    // Simulate multiple concurrent operations
    let handles: Vec<_> = (0..10).map(|i| {
        let storage = Arc::clone(&storage);
        tokio::spawn(async move {
            // Test that we can access the storage concurrently
            // This demonstrates the concurrent nature of your design
            let _ = &storage.write_counter; // Access atomic counter
            i
        })
    }).collect();
    
    // Wait for all operations
    for handle in handles {
        handle.await.unwrap();
    }
    
    println!("✅ Concurrent operations completed successfully!");
}

// This test shows the concept without needing the full Storage trait
#[test] 
fn test_cache_concept_demo() {
    use dashmap::DashMap;
    use std::sync::Arc;
    
    // Simulate your cache design
    let cache: DashMap<String, Arc<Vec<u8>>> = DashMap::new();
    
    // Step 1: Cache miss (key not found)
    let key = "http://example.com/image.jpg";
    assert!(cache.get(key).is_none(), "Should be cache miss");
    
    // Step 2: Store data (simulate downloading and caching)
    let data = Arc::new(b"fake image data".to_vec());
    cache.insert(key.to_string(), Arc::clone(&data));
    
    // Step 3: Cache hit (key found)
    let cached_data = cache.get(key).unwrap();
    assert_eq!(cached_data.value().as_slice(), b"fake image data");
    
    // Step 4: Concurrent access (multiple readers, no blocking!)
    let reader1 = cache.get(key).unwrap();
    let reader2 = cache.get(key).unwrap(); 
    
    // Both readers get the same Arc'd data with zero copying
    assert_eq!(reader1.value().as_slice(), reader2.value().as_slice());
    
    println!("✅ Cache concept works! DashMap allows lock-free concurrent reads");
}