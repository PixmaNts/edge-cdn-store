# Edge CDN Store Architecture and Sequence Diagrams

This document provides visual representations of the `edge-cdn-store` architecture and key request flows.

## 1. Architecture Schematic

```mermaid
graph LR
    subgraph "Edge CDN Store"
        EdgeStorage["EdgeStorage"]
        DashMap["DashMap<br>(Metadata & In-Progress)"]
        DiskStorage["DiskStorage (Tokio fs / io_uring*)"]
        EdgeStorage --> DashMap
        EdgeStorage --> DiskStorage
    end

    Client["Client"]
    PingoraServer["Pingora Server"]
    PingoraCacheModule["Pingora Cache Module"]
    OriginServer["Origin Server"]
    Disk["Disk Storage"]
    KernelPageCache["Kernel Page Cache"]

    Client --> PingoraServer
    PingoraServer --> PingoraCacheModule
    PingoraCacheModule --> EdgeStorage
    EdgeStorage --> OriginServer
    DiskStorage <--> Disk
    Disk <--> KernelPageCache
    KernelPageCache <--> DiskStorage
```

## 2. Sequence Diagrams

### 2.1. Scenario: Hit in Memory (Kernel Page Cache)

This diagram illustrates a request where the data is already present in the operating system's kernel page cache.

```mermaid
sequenceDiagram
    actor Client
    participant EdgeStorage
    participant DashMap as DashMap<br>(Metadata)
    participant DiskStorage
    participant KernelPageCache
    participant Disk

    Client->>EdgeStorage: Request (e.g., GET /resource)
    EdgeStorage->>DashMap: lookup(key)
    DashMap-->>EdgeStorage: CacheObject::OnDisk(metadata)
    EdgeStorage->>DiskStorage: read(file_path)
    DiskStorage->>KernelPageCache: read_block(offset)
    KernelPageCache-->>DiskStorage: Data (HIT)
    DiskStorage-->>EdgeStorage: Data
    EdgeStorage-->>Client: Serve Response
```

### 2.2. Scenario: Hit on Disk (Not in Kernel Page Cache Initially)

This diagram illustrates a request where the data is on disk but not yet in the kernel page cache.

```mermaid
sequenceDiagram
    actor Client
    participant EdgeStorage
    participant DashMap as DashMap<br>(Metadata)
    participant DiskStorage
    participant KernelPageCache
    participant Disk

    Client->>EdgeStorage: Request (e.g., GET /resource)
    EdgeStorage->>DashMap: lookup(key)
    DashMap-->>EdgeStorage: CacheObject::OnDisk(metadata)
    EdgeStorage->>DiskStorage: read(file_path)
    DiskStorage->>KernelPageCache: read_block(offset)
    KernelPageCache->>Disk: Fetch Data (MISS)
    Disk-->>KernelPageCache: Data
    KernelPageCache-->>DiskStorage: Data (now in cache)
    DiskStorage-->>EdgeStorage: Data
    EdgeStorage-->>Client: Serve Response
```

### 2.3. Scenario: Miss and Loading Process

This diagram illustrates a cache miss, fetching from the origin, buffering in-progress data, and asynchronous writing to disk. It also shows a concurrent request for the same resource hitting the in-progress buffer.

```mermaid
sequenceDiagram
    actor Client1 as Client (Initial Request)
    actor Client2 as Client (Concurrent Request)
    participant EdgeStorage
    participant DashMap as DashMap<br>(Metadata & In-Progress)
    participant OriginServer
    participant DiskStorage
    participant Disk

    Note over Client1,Disk: Initial Request (Miss)
    Client1->>EdgeStorage: Request (e.g., GET /new_resource)
    EdgeStorage->>DashMap: lookup(key)
    DashMap-->>EdgeStorage: Not Found (MISS)
    EdgeStorage->>OriginServer: Fetch /new_resource
    OriginServer-->>EdgeStorage: Stream Data (Chunk 1)
    EdgeStorage->>DashMap: create_in_progress(key, Chunk 1)
    EdgeStorage->>DiskStorage: async_write(key, Chunk 1)

    Note over Client2,Disk: Concurrent Request (Hit In-Progress)
    Client2->>EdgeStorage: Request (e.g., GET /new_resource)
    EdgeStorage->>DashMap: lookup(key)
    DashMap-->>EdgeStorage: CacheObject::InProgress(buffer)
    EdgeStorage-->>Client2: Serve Response (from in-progress buffer)

    Note over OriginServer,Disk: Continue Initial Request & Write
    OriginServer-->>EdgeStorage: Stream Data (Chunk N)
    EdgeStorage->>DashMap: append_in_progress(key, Chunk N)
    EdgeStorage->>DiskStorage: async_write(key, Chunk N)

    Note right of Disk: ... (more data streaming and writing) ...

    OriginServer-->>EdgeStorage: End of Stream
    DiskStorage-->>EdgeStorage: finalize_write(key)
    DiskStorage-->>EdgeStorage: Write Complete
    EdgeStorage->>DashMap: update_to_on_disk(key, file_path)
    EdgeStorage-->>Client1: Serve Response (final part)
```

Notes:
- The MVP uses `tokio::fs` for DiskStorage; an optional, experimental `io_uring` path is available for disk reads and streaming writes. Finalize/fsync currently uses Tokio APIs.
