//! Async Tokio-based Binder device implementation.
//!
//! Architecture:
//! - Single writer task per device (handles outgoing transactions)
//! - Multiple service tasks (one per worker, handle incoming via epoll)
//! - Arc<BinderDevice> shared across all tasks
//! - DashMap for thread-safe pending_replies and service_handlers

use rustix::io::{self};
use std::marker::PhantomData;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix;
use std::path::Path;
use std::process::id;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::{ptr, slice};
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

use crate::error::{Error, Result};
use crate::object::BinderObject;
use crate::sys::{
    BinderCommand, BinderFrozenStateInfo, BinderPtrCookie, BinderReturn, BinderSizeT,
    BinderTransactionData, BinderTransactionDataSecCtx, BinderType, BinderUintptrT,
    BinderWriteRead, FlatBinderFlags, FlatBinderObject, FlatBinderObjectData, SetContextMGR,
    TransactionFlags, TransactionTarget,
};
use crate::transaction::{Payload, Transaction};
use crate::BinderObjectEntry;
use dashmap::DashMap;

/// Shared binder device state.
pub struct BinderDevice {
    fd: Arc<OwnedFd>,
    pub(crate) cookie_counter: AtomicU64,
    looper_threads: Vec<std::thread::JoinHandle<()>>,
    exiting: Arc<AtomicBool>,
    pub(crate) service_handlers: Arc<DashMap<BinderUintptrT, Arc<dyn DynTransactionHandler>>>,
}

impl BinderDevice {
    pub fn new(path: impl AsRef<Path>) {}
    /// Create a new BinderDevice from an already-open fd.
    pub fn from_fd(fd: impl Into<OwnedFd>) -> Arc<Self> {
        let fd = Arc::new(fd.into());
        let handlers = Arc::new(DashMap::new());
        let exiting = Arc::new(AtomicBool::new(false));
        let loopers = (0..5)
            .map(|_| {
                std::thread::spawn({
                    let handlers = handlers.clone();
                    let runtime = tokio::runtime::Handle::current();
                    let exiting = exiting.clone();
                    let fd = fd.clone();
                    move || {
                        let _guard = runtime.enter();
                        looper(handlers, &runtime, exiting, fd)
                    }
                })
            })
            .collect();
        let device = Self {
            fd,
            cookie_counter: AtomicU64::new(1),
            looper_threads: loopers,
            exiting,
            service_handlers: handlers,
        };

        // let pending_replies = Arc::new(DashMap::new());
        // let service_handlers = Arc::new(DashMap::new());
        //
        // let (writer_tx, writer_rx) = mpsc::channel(64);
        // let writer_task = tokio::task::spawn(Self::spawn_writer_task(
        //     fd,
        //     writer_rx,
        //     Arc::clone(&pending_replies),
        //     Arc::clone(&service_handlers),
        // ));
        //
        // let num_workers = Handle::current().metrics().num_workers();
        // let service_tasks = (0..num_workers)
        //     .map(|_| {
        //         tokio::spawn(Self::service_task(
        //             device_key,
        //             Arc::clone(&pending_replies),
        //             Arc::clone(&service_handlers),
        //         ))
        //     })
        //     .collect();
        //
        // let device = Self {
        //     writer_tx,
        //     cookie_counter: AtomicU64::new(1),
        //     service_handlers,
        //     writer_task,
        //     service_tasks,
        // };
        //
        Arc::new(device)
    }

    /// Spawn the writer task that handles outgoing transactions.
    // async fn spawn_writer_task(
    //     fd: OwnedFd,
    //     mut writer_rx: mpsc::Receiver<WriterMessage>,
    //     pending_replies: Arc<DashMap<BinderUintptrT, PendingReply>>,
    //     _service_handlers: Arc<DashMap<BinderUintptrT, Arc<dyn DynTransactionHandler>>>,
    // ) {
    //     let async_fd = match AsyncFd::new(fd) {
    //         Ok(fd) => fd,
    //         Err(e) => {
    //             eprintln!("WriterTask: failed to create AsyncFd: {}", e);
    //             return;
    //         }
    //     };
    //
    //     loop {
    //         tokio::select! {
    //             msg = writer_rx.recv() => {
    //                 match msg {
    //                     Some(WriterMessage::Transact { target, code, payload, objects, cookie, reply_tx }) => {
    //                         // Self::send_transaction(&async_fd, target, code, &payload, &objects, cookie).await;
    //                         // pending_replies.insert(cookie, reply_tx);
    //                     }
    //                     Some(WriterMessage::TransactOneWay { target, code, payload, objects }) => {
    //                         // Self::send_transaction_one_way(&async_fd, target, code, &payload, &objects).await;
    //                     }
    //                     Some(WriterMessage::SetContextManager { cookie }) => {
    //                         // Self::_set_context_manager(&async_fd, cookie).await;
    //                     }
    //                     None => {
    //                         break;
    //                     }
    //                 }
    //             }
    //             _ = async_fd.readable() => {
    //                 // Handle any incoming (shouldn't happen in writer)
    //             }
    //         }
    //     }
    // }

    // /// Service task: handles incoming transactions on one worker thread.
    // async fn service_task(
    //     device_key: DeviceKey,
    //     pending_replies: Arc<DashMap<BinderUintptrT, PendingReply>>,
    //     service_handlers: Arc<DashMap<BinderUintptrT, Arc<dyn DynTransactionHandler>>>,
    // ) {
    //     if let Err(e) = binder_thread::ensure_device_ready(device_key.0) {
    //         eprintln!(
    //             "ServiceTask: failed to register device {:?}: {}",
    //             device_key, e
    //         );
    //         return;
    //     }
    //
    //     let async_fd = match binder_thread::get_device_async_fd(device_key) {
    //         Some(fd) => fd,
    //         None => {
    //             eprintln!("ServiceTask: failed to get async fd for {:?}", device_key);
    //             return;
    //         }
    //     };
    //
    //     loop {
    //         let mut guard = match async_fd.readable().await {
    //             Ok(guard) => guard,
    //             Err(e) => {
    //                 eprintln!("ServiceTask: AsyncFd error for {:?}: {}", device_key, e);
    //                 return;
    //             }
    //         };
    //
    //         match guard.try_io(|inner| Ok(Self::read_commands(inner.as_fd()))) {
    //             Ok(Ok(commands)) => {
    //                 for cmd in &commands {
    //                     Self::handle_command(
    //                         *cmd,
    //                         &async_fd,
    //                         Arc::clone(&pending_replies),
    //                         Arc::clone(&service_handlers),
    //                     )
    //                     .await;
    //                 }
    //             }
    //             Ok(Err(_)) => guard.clear_ready(),
    //             Err(_) => guard.clear_ready(),
    //         }
    //     }
    // }

    // /// Send a transaction to the kernel.
    // async fn send_transaction(
    //     async_fd: &AsyncFd<OwnedFd>,
    //     target: BinderRef,
    //     code: u32,
    //     payload: &[u8],
    //     objects: &[FlatBinderObject],
    //     _cookie: BinderUintptrT,
    // ) {
    //     let fd = async_fd.get_ref().as_fd();
    //
    //     let tx_data = TransactionData::new(target, code, TransactionFlags::empty());
    //     let (data, _existing_objects) = tx_data.with_payload(payload).build();
    //
    //     let objects_bytes = TransactionData::serialize_objects(objects);
    //
    //     let mut write_buf = Vec::with_capacity(4 + data.len() + objects_bytes.len());
    //     write_buf.extend_from_slice(&BC_TRANSACTION.to_le_bytes());
    //     write_buf.extend_from_slice(&data);
    //     write_buf.extend_from_slice(&objects_bytes);
    //
    //     let wr = BinderWriteRead {
    //         write_size: write_buf.len() as BinderSizeT,
    //         write_consumed: 0,
    //         write_buffer: write_buf.as_ptr() as BinderUintptrT,
    //         read_size: 0,
    //         read_consumed: 0,
    //         read_buffer: 0,
    //     };
    //
    //     let res = unsafe { rustix::ioctl::ioctl(fd, wr) };
    //     if let Err(e) = res {
    //         eprintln!("send_transaction error: {:?}", e);
    //     }
    // }

    // /// Send a one-way transaction.
    // async fn send_transaction_one_way(
    //     async_fd: &AsyncFd<OwnedFd>,
    //     target: BinderRef,
    //     code: u32,
    //     payload: &[u8],
    //     objects: &[FlatBinderObject],
    // ) {
    //     let fd = async_fd.get_ref().as_fd();
    //
    //     let tx_data = TransactionData::new(target, code, TransactionFlags::ONE_WAY);
    //     let (data, _existing_objects) = tx_data.with_payload(payload).build();
    //
    //     let objects_bytes = TransactionData::serialize_objects(objects);
    //
    //     let mut write_buf = Vec::with_capacity(4 + data.len() + objects_bytes.len());
    //     write_buf.extend_from_slice(&BC_TRANSACTION.to_le_bytes());
    //     write_buf.extend_from_slice(&data);
    //     write_buf.extend_from_slice(&objects_bytes);
    //
    //     let wr = BinderWriteRead {
    //         write_size: write_buf.len() as BinderSizeT,
    //         write_consumed: 0,
    //         write_buffer: write_buf.as_ptr() as BinderUintptrT,
    //         read_size: 0,
    //         read_consumed: 0,
    //         read_buffer: 0,
    //     };
    //
    //     let res = unsafe { rustix::ioctl::ioctl(fd, wr) };
    //     if let Err(e) = res {
    //         eprintln!("send_transaction_one_way error: {:?}", e);
    //     }
    // }

    pub async fn set_context_manager<T: TransactionHandler>(
        &self,
        handler: &BinderObject<T>,
    ) -> Result<()> {
        let buf = SetContextMGR(FlatBinderObject {
            hdr: crate::sys::BinderObjectHeader {
                type_: BinderType::BINDER,
            },
            flags: FlatBinderFlags::empty(),
            // TODO: don't hardcode this to 0?
            data: FlatBinderObjectData { binder: 0 },
            cookie: handler.cookie(),
            _lifetime: PhantomData::default(),
        });

        let res = unsafe { rustix::ioctl::ioctl(&self.fd, buf) };
        if let Err(e) = &res {
            eprintln!("send_transaction_one_way error: {:?}", e);
        }
        // TODO: find more accurate error
        res.map_err(|_| Error::PermissionDenied)
    }

    // /// Send a BC_REPLY with the given payload.
    // async fn send_reply(async_fd: &AsyncFd<OwnedFd>, data: &[u8], objects: &[FlatBinderObject]) {
    //     let fd = async_fd.get_ref().as_fd();
    //
    //     let tx_data = TransactionData::new(BinderRef(0), 0, TransactionFlags::ONE_WAY);
    //     let (reply_data, _existing_objects) = tx_data.with_payload(data).build();
    //
    //     let objects_bytes = TransactionData::serialize_objects(objects);
    //
    //     let mut write_buf = Vec::with_capacity(4 + reply_data.len() + objects_bytes.len());
    //     write_buf.extend_from_slice(&BC_REPLY.to_le_bytes());
    //     write_buf.extend_from_slice(&reply_data);
    //     write_buf.extend_from_slice(&objects_bytes);
    //
    //     let wr = BinderWriteRead {
    //         write_size: write_buf.len() as BinderSizeT,
    //         write_consumed: 0,
    //         write_buffer: write_buf.as_ptr() as BinderUintptrT,
    //         read_size: 0,
    //         read_consumed: 0,
    //         read_buffer: 0,
    //     };
    //
    //     let res = unsafe { rustix::ioctl::ioctl(fd, wr) };
    //     if let Err(e) = res {
    //         eprintln!("send_reply error: {:?}", e);
    //     }
    // }

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

    // /// Send a two-way transaction and wait for reply.
    // pub async fn transact(&self, target: BinderRef, code: u32, data: &[u8]) -> Result<Payload> {
    //     self.transact_with_objects(target, code, data, &[]).await
    // }

    // /// Send a two-way transaction with binder objects and wait for reply.
    // pub async fn transact_with_objects(
    //     &self,
    //     target: BinderRef,
    //     code: u32,
    //     data: &[u8],
    //     objects: &[FlatBinderObject],
    // ) -> Result<Payload> {
    //     let cookie = self.cookie_counter.fetch_add(1, Ordering::Relaxed);
    //     let (reply_tx, reply_rx) = oneshot::channel();
    //
    //     let payload = data.to_vec();
    //
    //     // self.writer_tx
    //     //     .send(WriterMessage::Transact {
    //     //         target,
    //     //         code,
    //     //         payload,
    //     //         objects: objects.to_vec(),
    //     //         cookie,
    //     //         reply_tx,
    //     //     })
    //     //     .await
    //     //     .map_err(|_| Error::Shutdown)?;
    //
    //     reply_rx.await.map_err(|_| Error::Shutdown)?
    // }

    // /// Send a one-way transaction (fire-and-forget).
    // pub fn transact_one_way(&self, target: BinderRef, code: u32, data: &[u8]) -> Result<()> {
    //     self.transact_one_way_with_objects(target, code, data, &[])
    // }

    // /// Send a one-way transaction with binder objects (fire-and-forget).
    // pub fn transact_one_way_with_objects(
    //     &self,
    //     target: BinderRef,
    //     code: u32,
    //     data: &[u8],
    //     objects: &[FlatBinderObject],
    // ) -> Result<()> {
    //     // let payload = data.to_vec();
    //     // self.writer_tx
    //     //     .try_send(WriterMessage::TransactOneWay {
    //     //         target,
    //     //         code,
    //     //         payload,
    //     //         objects: objects.to_vec(),
    //     //     })
    //     //     .map_err(|_| Error::Shutdown)
    //     todo!()
    // }
}

impl Drop for BinderDevice {
    fn drop(&mut self) {
        self.exiting.store(true, Ordering::Relaxed);
    }
}

unsafe fn write_binder_command(fd: BorrowedFd, data: &[u8]) -> rustix::io::Result<()> {
    let mut write_data = [0u8; 256];
    let mut read_data = [0u8; 256];
    let mut binder_wr = BinderWriteRead {
        write_size: write_data.len() as BinderSizeT,
        write_consumed: 0,
        write_buffer: write_data.as_mut_ptr() as BinderUintptrT,
        read_size: read_data.len() as BinderSizeT,
        read_consumed: 0,
        read_buffer: read_data.as_mut_ptr() as BinderUintptrT,
    };
    io::retry_on_intr(|| unsafe { rustix::ioctl::ioctl(fd, &mut binder_wr) })
}

fn looper(
    handlers: Arc<DashMap<BinderUintptrT, Arc<dyn DynTransactionHandler>>>,
    runtime: &tokio::runtime::Handle,
    exiting: Arc<AtomicBool>,
    dev_fd: Arc<OwnedFd>,
) {
    unsafe {
        write_binder_command(
            dev_fd.as_fd(),
            &BinderCommand::REGISTER_LOOPER.as_u32().to_ne_bytes(),
        )
        .unwrap();
        write_binder_command(
            dev_fd.as_fd(),
            &BinderCommand::ENTER_LOOPER.as_u32().to_ne_bytes(),
        )
        .unwrap();
    }
    loop {
        if exiting.load(Ordering::Relaxed) {
            break;
        }
        let mut write_data = [0u8; 256];
        let mut read_data = [0u8; 256];
        let mut binder_wr = BinderWriteRead {
            write_size: write_data.len() as BinderSizeT,
            write_consumed: 0,
            write_buffer: write_data.as_mut_ptr() as BinderUintptrT,
            read_size: read_data.len() as BinderSizeT,
            read_consumed: 0,
            read_buffer: read_data.as_mut_ptr() as BinderUintptrT,
        };
        let res = io::retry_on_intr(|| unsafe { rustix::ioctl::ioctl(&dev_fd, &mut binder_wr) });
        if let Err(err) = res {
            error!("binder write_read call failed: {err}");
            continue;
        }

        let read_slice = &read_data[0..binder_wr.read_consumed as usize];
        let write_slice = &write_data[0..binder_wr.write_consumed as usize];
        debug!("got: {:x?}, sent: {:x?}", read_slice, write_slice);
        let header = size_of::<u32>();
        let ret = BinderReturn::from_u32(unsafe { read_from_slice(&read_slice[..header]) });
        match ret {
            BinderReturn::ERROR => {
                let err = unsafe { read_from_slice::<i32>(&read_slice[header..]) };
                error!("received binder error: {err}");
            }
            BinderReturn::OK => {
                debug!("received ok");
            }
            BinderReturn::TRANSACTION_SEC_CTX | BinderReturn::TRANSACTION => {
                let (sec_ctx, transaction) = if ret == BinderReturn::TRANSACTION_SEC_CTX {
                    let v = unsafe {
                        read_from_slice::<BinderTransactionDataSecCtx>(&read_slice[header..])
                    };
                    (Some(v.sec_ctx), v.transaction_data)
                } else {
                    (None, unsafe {
                        read_from_slice::<BinderTransactionData>(&read_slice[header..])
                    })
                };
                // Safety: incomming transactions will always use the local identifier
                let target = unsafe { transaction.target.binder };
                let Some(handler) = handlers.get(&target) else {
                    warn!("unable to find handler for: {target:x?}");
                    continue;
                };
                let mut data = vec![0u8; transaction.data_size as usize];
                // let align = std::mem::align_of::<FlatBinder>();
                unsafe {
                    data.copy_from_slice(slice::from_raw_parts(
                        transaction.data.buffer as *const u8,
                        transaction.data_size as usize,
                    ));
                }
                let objects = unsafe {
                    slice::from_raw_parts(
                        transaction.data.buffer as *const FlatBinderObject,
                        transaction.offsets_size as usize / size_of::<FlatBinderObject>(),
                    )
                    .iter()
                    .map(|v| BinderObjectEntry::from_flat(v))
                    .collect::<Option<Vec<_>>>()
                };
                let Some(objects) = objects else {
                    error!("objects is None");
                    continue;
                };
                if transaction.flags.contains(TransactionFlags::ONE_WAY) {
                    runtime.block_on(handler.as_ref().handle_one_way(Transaction {
                        code: transaction.code,
                        payload: Payload { data, objects },
                    }));
                } else {
                    let mut reply = runtime.block_on(handler.as_ref().handle(Transaction {
                        code: transaction.code,
                        payload: Payload { data, objects },
                    }));
                    let obj_start_offset = reply.data.len();
                    for obj in reply.objects.iter().map(|v| v.to_flat()) {
                        unsafe {
                            let slice = slice::from_raw_parts(
                                &raw const obj as *const u8,
                                size_of::<FlatBinderObject>(),
                            );
                            reply.data.extend_from_slice(&slice);
                        }
                    }

                    let reply = BinderTransactionData {
                        // unused in reply
                        target: TransactionTarget { binder: 0 },
                        // unused in reply
                        cookie: 0,
                        code: transaction.code,
                        flags: transaction.flags,
                        sender_pid: rustix::process::getpid().as_raw_pid(),
                        sender_euid: rustix::process::getuid().as_raw(),
                        data_size: reply.data.len() as BinderSizeT,
                        offsets_size: (flat_reply_objs.len() * size_of::<FlatBinderObject>())
                            as BinderSizeT,
                        data: crate::sys::BinderTransactionDataPtrs {
                            buffer: reply.data.as_ptr() as u64,
                            offsets: 0,
                        },
                    };
                }
            }
            BinderReturn::REPLY => {
                let reply =
                    unsafe { read_from_slice::<BinderTransactionData>(&read_slice[header..]) };
            }
            BinderReturn::ACQUIRE_RESULT => {
                let v = unsafe { read_from_slice::<i32>(&read_slice[header..]) };
            }
            BinderReturn::DEAD_REPLY => {}
            BinderReturn::TRANSACTION_COMPLETE => {}
            BinderReturn::INCREFS => {
                let v = unsafe { read_from_slice::<BinderPtrCookie>(&read_slice[header..]) };
            }
            BinderReturn::ACQUIRE => {
                let v = unsafe { read_from_slice::<BinderPtrCookie>(&read_slice[header..]) };
            }
            BinderReturn::RELEASE => {
                let v = unsafe { read_from_slice::<BinderPtrCookie>(&read_slice[header..]) };
            }
            BinderReturn::DECREFS => {
                let v = unsafe { read_from_slice::<BinderPtrCookie>(&read_slice[header..]) };
            }
            BinderReturn::ATTEMPT_ACQUIRE => {
                let v = unsafe { read_from_slice::<BinderPtrCookie>(&read_slice[header..]) };
            }
            BinderReturn::NOOP => {}
            BinderReturn::SPAWN_LOOPER => {
                info!("binder requested additional looper");
            }
            BinderReturn::FINISHED => {}
            BinderReturn::DEAD_BINDER => {
                let v = unsafe { read_from_slice::<BinderUintptrT>(&read_slice[header..]) };
            }
            BinderReturn::CLEAR_DEATH_NOTIFICATION_DONE => {
                let v = unsafe { read_from_slice::<BinderUintptrT>(&read_slice[header..]) };
            }
            BinderReturn::FAILED_REPLY => {}
            BinderReturn::FROZEN_REPLY => {}
            BinderReturn::ONEWAY_SPAM_SUSPECT => {}
            BinderReturn::TRANSACTION_PENDING_FROZEN => {}
            BinderReturn::FROZEN_BINDER => {
                let v = unsafe { read_from_slice::<BinderFrozenStateInfo>(&read_slice[header..]) };
            }
            BinderReturn::CLEAR_FREEZE_NOTIFICATION_DONE => {
                let v = unsafe { read_from_slice::<BinderUintptrT>(&read_slice[header..]) };
            }
            msg_type => {
                error!("unknown binder message: {msg_type:?}");
            }
        }
    }
    // unsafe {
    //     rustix::ioctl::ioctl(&dev_fd, BcExitLooper);
    // }
    // TODO: figure out how the binder thread(not looper) exit call works
}
unsafe fn read_from_slice<T>(slice: &[u8]) -> T {
    assert!(slice.len() >= size_of::<T>());
    if slice.len() != size_of::<T>() {
        warn!("slice size doesn't match T size");
    }
    ptr::read_unaligned(slice.as_ptr().cast())
}

/// Message to the writer task.
// pub(crate) enum WriterMessage {
//     Transact {
//         target: BinderRef,
//         code: u32,
//         payload: Vec<u8>,
//         objects: Vec<FlatBinderObject>,
//         cookie: BinderUintptrT,
//         reply_tx: PendingReply,
//     },
//     TransactOneWay {
//         target: BinderRef,
//         code: u32,
//         payload: Vec<u8>,
//         objects: Vec<FlatBinderObject>,
//     },
//     SetContextManager {
//         cookie: u64,
//     },
// }

/// Pending reply for a two-way transaction.
type PendingReply = oneshot::Sender<Result<Payload>>;

#[async_trait::async_trait]
pub(crate) trait DynTransactionHandler: Send + Sync + 'static {
    async fn handle(&self, transaction: Transaction) -> Payload;
    async fn handle_one_way(&self, transaction: Transaction);
}

pub trait TransactionHandler: Send + Sync + 'static {
    fn handle(
        &self,
        transaction: Transaction,
    ) -> impl std::future::Future<Output = Payload> + std::marker::Send;

    fn handle_one_way(
        &self,
        transaction: Transaction,
    ) -> impl std::future::Future<Output = ()> + std::marker::Send;
}

#[async_trait::async_trait]
impl<T: TransactionHandler> DynTransactionHandler for T {
    async fn handle(&self, transaction: Transaction) -> Payload {
        <Self as TransactionHandler>::handle(&self, transaction).await
    }

    async fn handle_one_way(&self, transaction: Transaction) {
        <Self as TransactionHandler>::handle_one_way(&self, transaction).await
    }
}
