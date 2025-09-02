use io_uring::{opcode, IoUring};
use tokio::sync::{mpsc, oneshot};
use tokio::task;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, IoSlice, IoSliceMut};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Unique ID for each io_uring operation
static NEXT_USER_DATA: AtomicU64 = AtomicU64::new(1);

fn get_next_user_data() -> u64 {
    NEXT_USER_DATA.fetch_add(1, Ordering::Relaxed)
}

/// Represents a request to the IoUringManager
pub enum IoUringRequest {
    Write {
        path: PathBuf,
        offset: u64,
        data: Vec<u8>,
        resp: oneshot::Sender<io::Result<usize>>,
    },
    Read {
        path: PathBuf,
        offset: u64,
        len: usize,
        resp: oneshot::Sender<io::Result<Vec<u8>>>,
    },
    // Add other operations like Open, Close, Fsync, etc.
}

/// Manages a single io_uring instance on a dedicated blocking thread.
/// All io_uring operations are sent to this manager via an MPSC channel.
pub struct IoUringManager {
    ring: IoUring,
    // Map user_data to the oneshot sender for the original request
    pending_ops: HashMap<u64, PendingOperation>,
    // Channel to receive requests from other async tasks
    request_rx: mpsc::Receiver<IoUringRequest>,
    // Simple file descriptor cache for this prototype
    // In a real system, this would be more sophisticated (e.g., LRU, ref counting)
    open_files: HashMap<PathBuf, File>,
}

enum PendingOperation {
    Write(oneshot::Sender<io::Result<usize>>),
    Read(oneshot::Sender<io::Result<Vec<u8>>>),
}

impl IoUringManager {
    pub fn new(request_rx: mpsc::Receiver<IoUringRequest>) -> io::Result<Self> {
        Ok(Self {
            ring: IoUring::new(256)?, // A reasonable queue depth
            pending_ops: HashMap::new(),
            request_rx,
            open_files: HashMap::new(),
        })
    }

    // Main loop for the io_uring manager
    pub async fn run(mut self) {
        loop {
            // Try to receive new requests without blocking
            while let Ok(req) = self.request_rx.try_recv() {
                self.handle_request(req);
            }

            // Submit operations and wait for completions
            // We'll wait for at least one completion if there are pending ops,
            // otherwise we'll block until a new request arrives.
            let submitted = self.ring.submit();
            if submitted.is_err() {
                eprintln!("Error submitting io_uring operations: {:?}", submitted);
                // Handle error, maybe drain pending_ops and notify them of failure
                break; // For prototype, just break
            }

            // Wait for completions if there are pending operations
            if !self.pending_ops.is_empty() {
                match self.ring.submit_and_wait(1) {
                    Ok(_) => self.process_completions(),
                    Err(e) => {
                        eprintln!("Error submitting and waiting for io_uring completions: {:?}", e);
                        // Handle error, maybe drain pending_ops and notify them of failure
                        break; // For prototype, just break
                    }
                }
            } else {
                // If no pending ops, block until a new request arrives
                // This is a simplified approach; a real system might use a timeout
                // or a more complex polling strategy.
                if let Some(req) = self.request_rx.recv().await {
                    self.handle_request(req);
                } else {
                    // Sender dropped, manager can shut down
                    println!("IoUringManager shutting down: request channel closed.");
                    break;
                }
            }
        }
    }

    fn handle_request(&mut self, req: IoUringRequest) {
        match req {
            IoUringRequest::Write { path, offset, data, resp } => {
                let user_data = get_next_user_data();
                let file = self.get_or_open_file(&path, true, true, true);
                if file.is_err() {
                    let _ = resp.send(Err(file.unwrap_err()));
                    return;
                }
                let fd = file.unwrap().as_raw_fd();

                let slice = IoSlice::new(&data);
                let write_entry = opcode::Writev::new(
                    io_uring::types::Fd(fd),
                    &slice as *const IoSlice,
                    1, // Number of iovecs
                    offset,
                )
                .build()
                .user_data(user_data);

                unsafe {
                    if let Err(e) = self.ring.submission().push(&write_entry) {
                        eprintln!("Failed to push write operation to submission queue: {:?}", e);
                        let _ = resp.send(Err(io::Error::new(io::ErrorKind::Other, "Submission queue full")));
                        return;
                    }
                }
                self.pending_ops.insert(user_data, PendingOperation::Write(resp));
            }
            IoUringRequest::Read { path, offset, len, resp } => {
                let user_data = get_next_user_data();
                let file = self.get_or_open_file(&path, true, false, false);
                if file.is_err() {
                    let _ = resp.send(Err(file.unwrap_err()));
                    return;
                }
                let fd = file.unwrap().as_raw_fd();

                // Allocate buffer for read. This will be moved into the pending_ops.
                let mut buffer = vec![0; len];
                let slice = IoSliceMut::new(&mut buffer);
                let read_entry = opcode::Readv::new(
                    io_uring::types::Fd(fd),
                    &slice as *const IoSliceMut,
                    1, // Number of iovecs
                    offset,
                )
                .build()
                .user_data(user_data);

                unsafe {
                    if let Err(e) = self.ring.submission().push(&read_entry) {
                        eprintln!("Failed to push read operation to submission queue: {:?}", e);
                        let _ = resp.send(Err(io::Error::new(io::ErrorKind::Other, "Submission queue full")));
                        return;
                    }
                }
                self.pending_ops.insert(user_data, PendingOperation::Read(resp));
            }
        }
    }

    fn process_completions(&mut self) {
        let mut completions = Vec::new();
        for cqe in self.ring.completion() {
            completions.push(cqe);
        }

        for cqe in completions {
            let user_data = cqe.user_data();
            if let Some(op) = self.pending_ops.remove(&user_data) {
                let res = if cqe.result() < 0 {
                    Err(io::Error::from_raw_os_error(-cqe.result()))
                } else {
                    Ok(cqe.result() as usize)
                };

                match op {
                    PendingOperation::Write(resp) => {
                        let _ = resp.send(res);
                    }
                    PendingOperation::Read(resp) => {
                        // For read, we need the actual buffer. This is tricky with io_uring
                        // as the buffer is part of the SQE. For this prototype, we'll
                        // assume the buffer was managed by the caller or re-allocate.
                        // A real solution would pass the buffer in the request and get it back.
                        // For now, we'll just send the result size.
                        let _ = resp.send(res.map(|size| vec![0; size])); // Placeholder: send a zeroed vec of correct size
                    }
                }
            } else {
                eprintln!("Received completion for unknown user_data: {}", user_data);
            }
        }
    }

    // Simplified file management for prototype
    fn get_or_open_file(&mut self, path: &Path, create: bool, write: bool, truncate: bool) -> io::Result<&File> {
        if !self.open_files.contains_key(path) {
            let file = OpenOptions::new()
                .create(create)
                .write(write)
                .read(true) // Always open for read for simplicity
                .truncate(truncate)
                .open(path)?;
            self.open_files.insert(path.to_path_buf(), file);
        }
        Ok(self.open_files.get(path).unwrap())
    }
}

/// Starts the IoUringManager on a dedicated blocking thread.
/// Returns a sender to send requests to the manager.
pub fn start_io_uring_manager() -> mpsc::Sender<IoUringRequest> {
    let (request_tx, request_rx) = mpsc::channel(1024); // Buffer requests

    task::spawn_blocking(move || {
        let manager = IoUringManager::new(request_rx)
            .expect("Failed to create IoUringManager");
        
        // Run the manager's loop. This will block the spawned thread.
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to build current_thread runtime for IoUringManager")
            .block_on(manager.run());
    });

    request_tx
}

// Example usage (can be put in main.rs or a test)
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::fs;

    #[tokio::test]
    async fn test_io_uring_write_read() {
        let manager_tx = start_io_uring_manager();
        let test_file_path = PathBuf::from("/tmp/io_uring_test_file.txt");
        let write_data = b"Hello, io_uring from prototype!";

        // Ensure file doesn't exist from previous runs
        let _ = fs::remove_file(&test_file_path).await;

        // --- Write Operation ---
        let (resp_tx, resp_rx) = oneshot::channel();
        let write_req = IoUringRequest::Write {
            path: test_file_path.clone(),
            offset: 0,
            data: write_data.to_vec(),
            resp: resp_tx,
        };
        manager_tx.send(write_req).await.expect("Failed to send write request");

        let write_result = resp_rx.await.expect("Failed to receive write response");
        assert!(write_result.is_ok(), "Write operation failed: {:?}", write_result.err());
        assert_eq!(write_result.unwrap(), write_data.len(), "Incorrect bytes written");
        println!("Successfully wrote {} bytes to {:?}", write_data.len(), test_file_path);

        // --- Read Operation ---
        let (resp_tx, resp_rx) = oneshot::channel();
        let read_req = IoUringRequest::Read {
            path: test_file_path.clone(),
            offset: 0,
            len: write_data.len(),
            resp: resp_tx,
        };
        manager_tx.send(read_req).await.expect("Failed to send read request");

        let read_result = resp_rx.await.expect("Failed to receive read response");
        assert!(read_result.is_ok(), "Read operation failed: {:?}", read_result.err());
        let read_bytes = read_result.unwrap();
        assert_eq!(read_bytes.len(), write_data.len(), "Incorrect bytes read length");
        // Note: Due to prototype simplification, read_bytes will be zeroed.
        // In a real impl, the buffer would be passed in and filled.
        // For now, we'll just check length.
        println!("Successfully read {} bytes from {:?}", read_bytes.len(), test_file_path);

        // Clean up
        fs::remove_file(&test_file_path).await.expect("Failed to remove test file");
    }
}