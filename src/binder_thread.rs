//! Per-thread binder device registration and receiver handling.
//!
//! This module provides TLS-based lazy registration of binder devices
//! for each Tokio worker thread. Each thread that needs to do binder I/O
//! will:
//! 1. Lazily initialize its TLS state on first access
//! 2. Duplicate the source fd for its exclusive use
//! 3. Send BC_REGISTER_LOOPER to the kernel
//! 4. Clean up with BC_EXIT_LOOPER when the thread exits
//!
//! Multiple binder devices are supported, keyed by their source fd.

use crate::sys::{
    binder_write_read, BinderUintptrT, BC_EXIT_LOOPER, BC_REGISTER_LOOPER, BC_TRANSACTION,
};
use rustix::fd::FromRawFd;
use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd, RawFd};
use tokio::io::unix::AsyncFd;

/// Keyed by the SOURCE fd (the fd passed to BinderDevice)
/// Multiple devices can be registered per thread.
#[derive(Clone, Copy, Hash, Eq, PartialEq, Debug)]
pub struct DeviceKey(pub RawFd);

/// Registration state for one device on one thread.
/// Holds the duplicated fd wrapped in AsyncFd for async I/O.
struct DeviceRegistration {
    async_fd: AsyncFd<OwnedFd>,
}

impl DeviceRegistration {
    /// Create a new registration by duplicating the source fd.
    fn new(source_fd: RawFd) -> std::io::Result<Self> {
        // Duplicate the fd for this thread's exclusive use
        let owned = unsafe { OwnedFd::from_raw_fd(libc::dup(source_fd)) };
        let async_fd = AsyncFd::new(owned)?;

        // Send BC_REGISTER_LOOPER to register this thread as a looper
        send_binder_command(async_fd.as_fd(), BC_REGISTER_LOOPER)?;

        Ok(Self { async_fd })
    }

    /// Send BC_EXIT_LOOPER on drop for automatic cleanup.
    fn unregister(self) {
        let _ = send_binder_command(self.async_fd.as_fd(), BC_EXIT_LOOPER);
        // async_fd dropped → owned_fd closed automatically
    }
}

/// Per-thread registry of all binder devices this thread uses.
struct ThreadBinderState {
    registrations: HashMap<DeviceKey, DeviceRegistration>,
}

impl ThreadBinderState {
    fn new() -> Self {
        Self {
            registrations: HashMap::new(),
        }
    }

    fn ensure_device(&mut self, key: DeviceKey) -> std::io::Result<&AsyncFd<OwnedFd>> {
        let reg = self.registrations.entry(key).or_insert_with(|| {
            DeviceRegistration::new(key.0).expect("Failed to register binder device on thread")
        });
        Ok(&reg.async_fd)
    }
}

impl Drop for ThreadBinderState {
    fn drop(&mut self) {
        // Clean up all device registrations for this thread
        for (_, reg) in self.registrations.drain() {
            reg.unregister();
        }
    }
}

thread_local! {
    /// TLS - lazily initialized on first access.
    /// Each Tokio worker thread has its own ThreadBinderState.
    static THREAD_STATE: std::cell::RefCell<Option<ThreadBinderState>> =
        const { std::cell::RefCell::new(None) };
}

/// Ensure the current thread is registered for the device identified by source_fd.
/// Returns a token that can be used to get the AsyncFd for this device.
pub fn ensure_device_ready(source_fd: RawFd) -> std::io::Result<DeviceKey> {
    let key = DeviceKey(source_fd);

    THREAD_STATE.with_borrow_mut(|cell| {
        let state = cell.get_or_insert_with(ThreadBinderState::new);
        state.ensure_device(key)?;
        Ok(key)
    })
}

/// Get the AsyncFd for a device key from TLS.
/// Must be called on the same thread that called ensure_device_ready.
pub fn get_device_async_fd(key: DeviceKey) -> Option<AsyncFd<OwnedFd>> {
    THREAD_STATE.with_borrow_mut(|cell| {
        if let Some(state) = cell.as_mut() {
            state.registrations.get(&key).and_then(|reg| {
                // Create a new AsyncFd wrapping the same OwnedFd
                // This is safe because we own the fd in the registration
                let owned = reg.async_fd.get_ref().try_clone().ok()?;
                AsyncFd::new(owned).ok()
            })
        } else {
            None
        }
    })
}

/// Spawn receiver tasks on ALL Tokio workers to handle incoming transactions.
/// Each worker thread will lazily register and process incoming binder commands.
pub fn spawn_receivers_on_all_workers(device_key: DeviceKey, source_fd: RawFd) {
    use tokio::runtime::Handle;

    let handle = Handle::current();
    let num_workers = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(4);

    for _ in 0..num_workers {
        handle.spawn(receiver_task(device_key, source_fd));
    }
}

/// Receiver task that handles incoming binder transactions on one worker thread.
async fn receiver_task(device_key: DeviceKey, source_fd: RawFd) {
    if let Err(e) = ensure_device_ready(source_fd) {
        eprintln!(
            "binder_thread: failed to register device {:?}: {}",
            device_key, e
        );
        return;
    }

    let async_fd = match get_device_async_fd(device_key) {
        Some(fd) => fd,
        None => {
            eprintln!(
                "binder_thread: failed to get device async fd for {:?}",
                device_key
            );
            return;
        }
    };

    loop {
        // Wait for EPOLLIN (readability) via AsyncFd
        let mut guard = match async_fd.readable().await {
            Ok(guard) => guard,
            Err(e) => {
                eprintln!("binder_thread: AsyncFd error for {:?}: {}", device_key, e);
                return;
            }
        };

        match guard.try_io(|inner| Ok(read_binder_commands(inner.as_fd()))) {
            Ok(Ok(commands)) => {
                for cmd in commands {
                    handle_command(cmd, &async_fd, device_key);
                }
            }
            Ok(Err(_)) => {
                // WouldBlock - clear ready and retry
                guard.clear_ready();
            }
            Err(_) => {
                guard.clear_ready();
            }
        }
    }
}

/// Read binder commands from the device via BINDER_WRITE_READ ioctl.
fn read_binder_commands(fd: std::os::fd::BorrowedFd<'_>) -> Vec<u32> {
    let read_buf_size = 256 * 1024;
    let mut read_buf = vec![0u8; read_buf_size];

    let wr = binder_write_read {
        write_size: 0,
        write_consumed: 0,
        write_buffer: 0,
        read_size: read_buf_size as BinderUintptrT,
        read_consumed: 0,
        read_buffer: read_buf.as_mut_ptr() as BinderUintptrT,
    };

    let res = unsafe { rustix::ioctl::ioctl(fd, wr) };

    match res {
        Ok(_) => {
            let consumed = wr.read_consumed as usize;
            if consumed >= 4 {
                let num_cmds = consumed / 4;
                let ptr = read_buf.as_ptr() as *const u32;
                (0..num_cmds).map(|i| unsafe { *ptr.add(i) }).collect()
            } else {
                Vec::new()
            }
        }
        Err(_) => Vec::new(),
    }
}

/// Handle a single binder command.
fn handle_command(cmd: u32, async_fd: &AsyncFd<OwnedFd>, device_key: DeviceKey) {
    use crate::sys::{
        BR_DEAD_REPLY, BR_ERROR, BR_NOOP, BR_REPLY, BR_SPAWN_LOOPER, BR_TRANSACTION,
        BR_TRANSACTION_COMPLETE,
    };

    match cmd {
        BR_NOOP => {
            // eprintln!("BR_NOOP");
        }
        BR_TRANSACTION => {
            // eprintln!("BR_TRANSACTION on device {:?}", device_key);
            if let Some(tx) = parse_br_transaction(async_fd) {
                handle_transaction(tx, async_fd, device_key);
            }
        }
        BR_TRANSACTION_COMPLETE => {
            // eprintln!("BR_TRANSACTION_COMPLETE");
        }
        BR_REPLY => {
            // eprintln!("BR_REPLY");
            // Handled by the waiting transact() call via channel
        }
        BR_DEAD_REPLY => {
            eprintln!("BR_DEAD_REPLY on device {:?}", device_key);
        }
        BR_SPAWN_LOOPER => {
            // eprintln!("BR_SPAWN_LOOPER - kernel requesting more loopers");
            // Could spawn more receiver tasks here if needed
        }
        BR_ERROR => {
            eprintln!("BR_ERROR on device {:?}", device_key);
        }
        _ => {
            eprintln!("Unknown binder command: 0x{:08x}", cmd);
        }
    }
}

/// Parse a BR_TRANSACTION command to extract transaction data.
fn parse_br_transaction(async_fd: &AsyncFd<OwnedFd>) -> Option<TransactionInfo> {
    use crate::sys::binder_transaction_data;

    let read_buf_size = 256 * 1024;
    let mut read_buf = vec![0u8; read_buf_size];

    let wr = binder_write_read {
        write_size: 0,
        write_consumed: 0,
        write_buffer: 0,
        read_size: read_buf_size as BinderUintptrT,
        read_consumed: 0,
        read_buffer: read_buf.as_mut_ptr() as BinderUintptrT,
    };

    let res = unsafe { rustix::ioctl::ioctl(async_fd.get_ref().as_fd(), wr) };

    if res.is_err() {
        return None;
    }

    let data = &read_buf[..wr.read_consumed as usize];
    if data.len() < std::mem::size_of::<binder_transaction_data>() {
        return None;
    }

    let data_size_offset = 7 * 8;

    let data_size = u64::from_le_bytes(
        data[data_size_offset..data_size_offset + 8]
            .try_into()
            .ok()?,
    );

    let data_start = std::mem::size_of::<binder_transaction_data>();
    let offsets_start = data_start + data_size as usize;

    let payload_data = if data_size > 0 {
        data[data_start..offsets_start].to_vec()
    } else {
        Vec::new()
    };

    let code = u32::from_le_bytes(data[4 * 8..4 * 8 + 4].try_into().ok()?);

    Some(TransactionInfo {
        code,
        payload: payload_data,
    })
}

/// Information extracted from a BR_TRANSACTION.
struct TransactionInfo {
    code: u32,
    payload: Vec<u8>,
}

/// Handle an incoming transaction.
/// TODO: Route to registered service handlers.
fn handle_transaction(tx: TransactionInfo, _async_fd: &AsyncFd<OwnedFd>, _device_key: DeviceKey) {
    eprintln!(
        "Received transaction code=0x{:08x}, size={}",
        tx.code,
        tx.payload.len()
    );
    // TODO: Look up registered handler for the target and dispatch
}

/// Send a single binder command (just the 4-byte command code).
fn send_binder_command(fd: std::os::fd::BorrowedFd<'_>, cmd: u32) -> std::io::Result<()> {
    let wr = binder_write_read {
        write_size: 4,
        write_consumed: 0,
        write_buffer: &cmd as *const u32 as BinderUintptrT,
        read_size: 0,
        read_consumed: 0,
        read_buffer: 0,
    };

    let res = unsafe { rustix::ioctl::ioctl(fd, wr) };
    match res {
        Ok(()) => Ok(()),
        Err(e) => {
            let errno = e.raw_os_error();
            Err(std::io::Error::from_raw_os_error(errno))
        }
    }
}

/// Send transaction data via BC_TRANSACTION.
pub async fn send_binder_write(async_fd: &AsyncFd<OwnedFd>, data: &[u8]) {
    let fd = async_fd.get_ref().as_fd();
    let cmd = BC_TRANSACTION;

    let mut write_buf = Vec::with_capacity(4 + data.len());
    write_buf.extend_from_slice(&cmd.to_le_bytes());
    write_buf.extend_from_slice(data);

    let wr = binder_write_read {
        write_size: write_buf.len() as BinderUintptrT,
        write_consumed: 0,
        write_buffer: write_buf.as_ptr() as BinderUintptrT,
        read_size: 0,
        read_consumed: 0,
        read_buffer: 0,
    };

    let res = unsafe { rustix::ioctl::ioctl(fd, wr) };
    if let Err(e) = res {
        eprintln!("send_binder_write error: {:?}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_key() {
        let key1 = DeviceKey(5);
        let key2 = DeviceKey(5);
        let key3 = DeviceKey(10);

        assert_eq!(key1, key2);
        assert_ne!(key1, key3);

        assert_eq!(key1.0, 5);
    }

    #[test]
    fn test_device_key_hash() {
        use std::collections::HashMap;

        let mut map: HashMap<DeviceKey, i32> = HashMap::new();
        map.insert(DeviceKey(5), 42);

        assert_eq!(map.get(&DeviceKey(5)), Some(&42));
        assert_eq!(map.get(&DeviceKey(10)), None);
    }
}
