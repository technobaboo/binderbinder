//! Naming convention, discovery, and cross-process locking for the
//! pre-provisioned binderfs device nodes used by the test suite.
//!
//! The nodes themselves are created once, as root, by
//! `examples/setup_test_pool.rs`. Everything in this module runs as a normal
//! user and only ever opens/locks nodes that already exist — no privileged
//! calls happen here. Shared by the in-crate `#[cfg(test)]` unit tests and by
//! the integration tests under `tests/support/mod.rs`.
#![doc(hidden)]

use std::fs::{File, OpenOptions};
use std::path::PathBuf;

pub const POOL_PREFIX: &str = "bb-test-";
pub const DEFAULT_POOL_SIZE: usize = 16;

pub fn pool_size() -> usize {
    std::env::var("BINDERBINDER_TEST_POOL_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_POOL_SIZE)
}

pub fn mount_path() -> PathBuf {
    std::env::var("BINDERBINDER_TEST_MOUNT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(crate::fs::DEFAULT_BINDERFS_PATH))
}

pub fn node_path(index: usize) -> PathBuf {
    mount_path().join(format!("{POOL_PREFIX}{index}"))
}

/// Lockfiles can't live under the binderfs mount itself — that directory is
/// root-owned (0755), so a normal user can open the 0666 device nodes inside
/// it but can't create new files there. Use a separate, writable directory
/// instead.
fn lock_dir() -> PathBuf {
    std::env::var("BINDERBINDER_TEST_LOCK_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("binderbinder-test-pool"))
}

fn lock_path(index: usize) -> PathBuf {
    lock_dir().join(format!("{POOL_PREFIX}{index}.lock"))
}

/// RAII claim on one pooled binder device node: holds an exclusive lock on a
/// companion lockfile for as long as this guard lives, so no other test
/// process/thread will try to become context manager on the same node at
/// the same time. The lock is released automatically on drop, and also by
/// the kernel if the holding process dies without unwinding cleanly.
pub struct PoolNode {
    pub index: usize,
    pub path: PathBuf,
    _lock: File,
}

impl PoolNode {
    /// Claims a free node from the pool, blocking if every node is
    /// currently held by another test. Panics with a clear message if the
    /// pool hasn't been provisioned (run `setup_test_pool` as root first).
    pub fn acquire() -> PoolNode {
        let size = pool_size();
        let mount = mount_path();
        assert!(
            mount.exists(),
            "binderfs test pool not found at {} — run `sudo cargo run --example setup_test_pool` first",
            mount.display()
        );

        // Stagger the starting index per-process so concurrent test
        // processes don't all probe node 0 first.
        let start = std::process::id() as usize;

        for offset in 0..size {
            let index = (start + offset) % size;
            if let Some(node) = Self::try_claim(index) {
                return node;
            }
        }

        // Whole pool was busy on the first pass — block on one node rather
        // than spin-polling.
        let index = start % size;
        Self::block_claim(index)
    }

    fn try_claim(index: usize) -> Option<PoolNode> {
        let path = node_path(index);
        if !path.exists() {
            return None;
        }
        let lock_file = open_lock_file(index);
        match lock_file.try_lock() {
            Ok(()) => Some(PoolNode {
                index,
                path,
                _lock: lock_file,
            }),
            Err(std::fs::TryLockError::WouldBlock) => None,
            Err(std::fs::TryLockError::Error(e)) => {
                panic!("failed to lock pool node {index}: {e}")
            }
        }
    }

    fn block_claim(index: usize) -> PoolNode {
        let path = node_path(index);
        assert!(
            path.exists(),
            "binderfs test pool node {index} not found at {} — run `sudo cargo run --example setup_test_pool` first",
            path.display()
        );
        let lock_file = open_lock_file(index);
        lock_file
            .lock()
            .unwrap_or_else(|e| panic!("failed to lock pool node {index}: {e}"));
        PoolNode {
            index,
            path,
            _lock: lock_file,
        }
    }
}

fn open_lock_file(index: usize) -> File {
    std::fs::create_dir_all(lock_dir())
        .unwrap_or_else(|e| panic!("could not create lock dir {}: {e}", lock_dir().display()));
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path(index))
        .unwrap_or_else(|e| panic!("could not open lockfile for pool node {index}: {e}"))
}
