//! Async Tokio-based Binder device implementation.
//!
//! Architecture:
//! - Single writer task per device (handles outgoing transactions)
//! - Multiple service tasks (one per worker, handle incoming via epoll)
//! - Arc<BinderDevice> shared across all tasks
//! - DashMap for thread-safe pending_replies and service_handlers

use std::os::fd::{AsFd, BorrowedFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::unix::AsyncFd;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::binder_ref::BinderRef;
use crate::binder_thread::{self, DeviceKey};
use crate::error::{Error, Result};
use crate::object::BinderObject;
use crate::sys::{
    binder_transaction_data, binder_write_read, BinderSizeT, BinderUintptrT, BC_REPLY,
    BC_TRANSACTION, BR_REPLY, BR_TRANSACTION, TF_ONE_WAY,
};
use crate::transaction::{Payload, Transaction, TransactionData};
use dashmap::DashMap;
use rustix::fd::FromRawFd;

/// Shared binder device state.
pub struct BinderDevice {
    pub(crate) source_fd: RawFd,
    pub(crate) device_key: DeviceKey,
    pub(crate) cookie_counter: AtomicU64,
    pub(crate) writer_tx: mpsc::Sender<WriterMessage>,
    pub(crate) pending_replies: Arc<DashMap<BinderUintptrT, PendingReply>>,
    pub(crate) service_handlers: Arc<DashMap<BinderUintptrT, Arc<dyn TransactionHandler>>>,
    pub(crate) writer_task: JoinHandle<()>,
    pub(crate) service_tasks: Vec<JoinHandle<()>>,
}

impl BinderDevice {
    /// Create a new BinderDevice from an already-open fd.
    pub fn new(fd: RawFd) -> Arc<Self> {
        let owned_fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let device_key = DeviceKey(fd);

        let pending_replies = Arc::new(DashMap::new());
        let service_handlers = Arc::new(DashMap::new());

        let (writer_tx, writer_rx) = mpsc::channel(64);
        let writer_task = tokio::task::spawn(Self::spawn_writer_task(
            owned_fd,
            writer_rx,
            Arc::clone(&pending_replies),
            Arc::clone(&service_handlers),
        ));

        let num_workers = Handle::current().metrics().num_workers();
        let service_tasks = (0..num_workers)
            .map(|_| {
                tokio::spawn(Self::service_task(
                    device_key,
                    Arc::clone(&pending_replies),
                    Arc::clone(&service_handlers),
                ))
            })
            .collect();

        let device = Self {
            source_fd: fd,
            device_key,
            writer_tx,
            cookie_counter: AtomicU64::new(1),
            pending_replies,
            service_handlers,
            writer_task,
            service_tasks,
        };

        Arc::new(device)
    }

    /// Spawn the writer task that handles outgoing transactions.
    async fn spawn_writer_task(
        fd: OwnedFd,
        mut writer_rx: mpsc::Receiver<WriterMessage>,
        pending_replies: Arc<DashMap<BinderUintptrT, PendingReply>>,
        _service_handlers: Arc<DashMap<BinderUintptrT, Arc<dyn TransactionHandler>>>,
    ) {
        let async_fd = match AsyncFd::new(fd) {
            Ok(fd) => fd,
            Err(e) => {
                eprintln!("WriterTask: failed to create AsyncFd: {}", e);
                return;
            }
        };

        loop {
            tokio::select! {
                msg = writer_rx.recv() => {
                    match msg {
                        Some(WriterMessage::Transact { target, code, payload, cookie, reply_tx }) => {
                            Self::send_transaction(&async_fd, target, code, &payload, cookie).await;
                            pending_replies.insert(cookie, reply_tx);
                        }
                        Some(WriterMessage::TransactOneWay { target, code, payload }) => {
                            Self::send_transaction_one_way(&async_fd, target, code, &payload).await;
                        }
                        None => {
                            break;
                        }
                    }
                }
                _ = async_fd.readable() => {
                    // Handle any incoming (shouldn't happen in writer)
                }
            }
        }
    }

    /// Service task: handles incoming transactions on one worker thread.
    async fn service_task(
        device_key: DeviceKey,
        pending_replies: Arc<DashMap<BinderUintptrT, PendingReply>>,
        service_handlers: Arc<DashMap<BinderUintptrT, Arc<dyn TransactionHandler>>>,
    ) {
        if let Err(e) = binder_thread::ensure_device_ready(device_key.0) {
            eprintln!(
                "ServiceTask: failed to register device {:?}: {}",
                device_key, e
            );
            return;
        }

        let async_fd = match binder_thread::get_device_async_fd(device_key) {
            Some(fd) => fd,
            None => {
                eprintln!("ServiceTask: failed to get async fd for {:?}", device_key);
                return;
            }
        };

        loop {
            let mut guard = match async_fd.readable().await {
                Ok(guard) => guard,
                Err(e) => {
                    eprintln!("ServiceTask: AsyncFd error for {:?}: {}", device_key, e);
                    return;
                }
            };

            match guard.try_io(|inner| Ok(Self::read_commands(inner.as_fd()))) {
                Ok(Ok(commands)) => {
                    for cmd in &commands {
                        Self::handle_command(
                            *cmd,
                            &async_fd,
                            Arc::clone(&pending_replies),
                            Arc::clone(&service_handlers),
                        )
                        .await;
                    }
                }
                Ok(Err(_)) => guard.clear_ready(),
                Err(_) => guard.clear_ready(),
            }
        }
    }

    /// Read binder commands from the device.
    fn read_commands(fd: BorrowedFd<'_>) -> Vec<u32> {
        let read_buf_size = 256 * 1024;
        let mut read_buf = vec![0u8; read_buf_size];

        let wr = binder_write_read {
            write_size: 0,
            write_consumed: 0,
            write_buffer: 0,
            read_size: read_buf_size as BinderSizeT,
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
    async fn handle_command(
        cmd: u32,
        async_fd: &AsyncFd<OwnedFd>,
        pending_replies: Arc<DashMap<BinderUintptrT, PendingReply>>,
        service_handlers: Arc<DashMap<BinderUintptrT, Arc<dyn TransactionHandler>>>,
    ) {
        match cmd {
            BR_TRANSACTION => {
                if let Some((cookie, code, payload_data)) = Self::parse_transaction(async_fd) {
                    eprintln!("BR_TRANSACTION: cookie=0x{:x}", cookie);
                    if let Some(handler_entry) = service_handlers.get(&cookie) {
                        let transaction = Transaction {
                            code,
                            payload: Payload::with_data(payload_data),
                        };
                        // Call handler asynchronously and send reply
                        let reply = handler_entry.handle(transaction).await;
                        Self::send_reply(async_fd, &reply.data).await;
                    } else {
                        eprintln!("No handler registered for cookie=0x{:x}", cookie);
                    }
                }
            }
            BR_REPLY => {
                if let Some((cookie, reply_data)) = Self::parse_reply(async_fd) {
                    eprintln!("BR_REPLY: cookie=0x{:x}", cookie);
                    if let Some((_, pending)) = pending_replies.remove(&cookie) {
                        let payload = Payload::with_data(reply_data);
                        let _ = pending.send(Ok(payload));
                    } else {
                        eprintln!("No pending reply for cookie=0x{:x}", cookie);
                    }
                }
            }
            _ => {}
        }
    }

    /// Parse a BR_TRANSACTION to extract cookie, code, and data.
    fn parse_transaction(async_fd: &AsyncFd<OwnedFd>) -> Option<(BinderUintptrT, u32, Vec<u8>)> {
        let read_buf_size = 256 * 1024;
        let mut read_buf = vec![0u8; read_buf_size];

        let wr = binder_write_read {
            write_size: 0,
            write_consumed: 0,
            write_buffer: 0,
            read_size: read_buf_size as BinderSizeT,
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

        let cookie_offset = 8;
        let data_size_offset = 7 * 8;
        let code_offset = 4 * 8;

        let cookie =
            BinderUintptrT::from_le_bytes(data[cookie_offset..cookie_offset + 8].try_into().ok()?);

        let code = u32::from_le_bytes(data[code_offset..code_offset + 4].try_into().ok()?);

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

        Some((cookie, code, payload_data))
    }

    /// Parse a BR_REPLY to extract cookie and data.
    fn parse_reply(async_fd: &AsyncFd<OwnedFd>) -> Option<(BinderUintptrT, Vec<u8>)> {
        let read_buf_size = 256 * 1024;
        let mut read_buf = vec![0u8; read_buf_size];

        let wr = binder_write_read {
            write_size: 0,
            write_consumed: 0,
            write_buffer: 0,
            read_size: read_buf_size as BinderSizeT,
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

        let cookie_offset = 8;
        let data_size_offset = 7 * 8;

        let cookie =
            BinderUintptrT::from_le_bytes(data[cookie_offset..cookie_offset + 8].try_into().ok()?);

        let data_size = u64::from_le_bytes(
            data[data_size_offset..data_size_offset + 8]
                .try_into()
                .ok()?,
        );

        let data_start = std::mem::size_of::<binder_transaction_data>();
        let payload_data = if data_size > 0 {
            data[data_start..data_start + data_size as usize].to_vec()
        } else {
            Vec::new()
        };

        Some((cookie, payload_data))
    }

    /// Send a transaction to the kernel.
    async fn send_transaction(
        async_fd: &AsyncFd<OwnedFd>,
        target: BinderRef,
        code: u32,
        payload: &[u8],
        _cookie: BinderUintptrT,
    ) {
        let fd = async_fd.get_ref().as_fd();

        let tx_data = TransactionData::new(target, code, 0);
        let (data, _objects) = tx_data.with_payload(payload).build();

        let mut write_buf = Vec::with_capacity(4 + data.len());
        write_buf.extend_from_slice(&BC_TRANSACTION.to_le_bytes());
        write_buf.extend_from_slice(&data);

        let wr = binder_write_read {
            write_size: write_buf.len() as BinderSizeT,
            write_consumed: 0,
            write_buffer: write_buf.as_ptr() as BinderUintptrT,
            read_size: 0,
            read_consumed: 0,
            read_buffer: 0,
        };

        let res = unsafe { rustix::ioctl::ioctl(fd, wr) };
        if let Err(e) = res {
            eprintln!("send_transaction error: {:?}", e);
        }
    }

    /// Send a one-way transaction.
    async fn send_transaction_one_way(
        async_fd: &AsyncFd<OwnedFd>,
        target: BinderRef,
        code: u32,
        payload: &[u8],
    ) {
        let fd = async_fd.get_ref().as_fd();

        let tx_data = TransactionData::new(target, code, TF_ONE_WAY);
        let (data, _objects) = tx_data.with_payload(payload).build();

        let mut write_buf = Vec::with_capacity(4 + data.len());
        write_buf.extend_from_slice(&BC_TRANSACTION.to_le_bytes());
        write_buf.extend_from_slice(&data);

        let wr = binder_write_read {
            write_size: write_buf.len() as BinderSizeT,
            write_consumed: 0,
            write_buffer: write_buf.as_ptr() as BinderUintptrT,
            read_size: 0,
            read_consumed: 0,
            read_buffer: 0,
        };

        let res = unsafe { rustix::ioctl::ioctl(fd, wr) };
        if let Err(e) = res {
            eprintln!("send_transaction_one_way error: {:?}", e);
        }
    }

    /// Send a BC_REPLY with the given payload.
    async fn send_reply(async_fd: &AsyncFd<OwnedFd>, data: &[u8]) {
        let fd = async_fd.get_ref().as_fd();

        let tx_data = TransactionData::new(BinderRef(0), 0, 0);
        let (reply_data, _objects) = tx_data.with_payload(data).build();

        let mut write_buf = Vec::with_capacity(4 + reply_data.len());
        write_buf.extend_from_slice(&BC_REPLY.to_le_bytes());
        write_buf.extend_from_slice(&reply_data);

        let wr = binder_write_read {
            write_size: write_buf.len() as BinderSizeT,
            write_consumed: 0,
            write_buffer: write_buf.as_ptr() as BinderUintptrT,
            read_size: 0,
            read_consumed: 0,
            read_buffer: 0,
        };

        let res = unsafe { rustix::ioctl::ioctl(fd, wr) };
        if let Err(e) = res {
            eprintln!("send_reply error: {:?}", e);
        }
    }

    /// Register a handler for incoming transactions and return a binder object.
    ///
    /// When the returned `BinderObject` is dropped, the handler is automatically
    /// unregistered from the device (RAII pattern).
    pub fn register_object<T: TransactionHandler>(self: &Arc<Self>, handler: T) -> BinderObject<T> {
        let cookie = self.cookie_counter.fetch_add(1, Ordering::Relaxed);

        let handler = Arc::new(handler);

        self.service_handlers.insert(cookie, handler.clone());

        BinderObject {
            device: Arc::downgrade(self),
            handler,
            cookie,
        }
    }

    /// Send a two-way transaction and wait for reply.
    pub async fn transact(&self, target: BinderRef, code: u32, data: &[u8]) -> Result<Payload> {
        let cookie = self.cookie_counter.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = oneshot::channel();

        let payload = data.to_vec();

        self.writer_tx
            .send(WriterMessage::Transact {
                target,
                code,
                payload,
                cookie,
                reply_tx,
            })
            .await
            .map_err(|_| Error::Shutdown)?;

        reply_rx.await.map_err(|_| Error::Shutdown)?
    }

    /// Send a one-way transaction (fire-and-forget).
    pub fn transact_one_way(&self, target: BinderRef, code: u32, data: &[u8]) -> Result<()> {
        let payload = data.to_vec();
        self.writer_tx
            .try_send(WriterMessage::TransactOneWay {
                target,
                code,
                payload,
            })
            .map_err(|_| Error::Shutdown)
    }
}

impl Drop for BinderDevice {
    fn drop(&mut self) {
        self.writer_task.abort();
        for task in &self.service_tasks {
            task.abort();
        }
    }
}

/// Message to the writer task.
pub(crate) enum WriterMessage {
    Transact {
        target: BinderRef,
        code: u32,
        payload: Vec<u8>,
        cookie: BinderUintptrT,
        reply_tx: PendingReply,
    },
    TransactOneWay {
        target: BinderRef,
        code: u32,
        payload: Vec<u8>,
    },
}

/// Pending reply for a two-way transaction.
type PendingReply = oneshot::Sender<Result<Payload>>;

#[async_trait::async_trait]
pub trait TransactionHandler: Send + Sync + 'static {
    async fn handle(&self, transaction: Transaction) -> Payload;
}
