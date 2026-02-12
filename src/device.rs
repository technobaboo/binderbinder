//! Async Tokio-based Binder device implementation.
//!
//! Architecture:
//! - Single writer task per device (handles outgoing transactions)
//! - Multiple service tasks (one per worker, handle incoming via epoll)
//! - Arc<BinderDevice> shared across all tasks
//! - DashMap for thread-safe pending_replies and service_handlers

use core::slice;
use rustix::fs::{Mode, OFlags};
use rustix::io::{self, Errno};
use rustix::mm::{mmap, munmap, MapFlags, ProtFlags};
use std::ffi::c_void;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::thread::sleep;
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, error, info, trace, warn};

use crate::binder_object::{
    BinderObject, BinderObjectId, BinderObjectOrRef, BinderRef, WeakBinderRef,
};
use crate::error::{Error, Result};
use crate::payload::{PayloadBuilder, PayloadReader};
use crate::sys::{
    BinderCommand, BinderExtendedError, BinderFrozenStateInfo, BinderPtrCookie, BinderReturn,
    BinderSizeT, BinderTransactionData, BinderTransactionDataSecCtx, BinderUintptrT,
    BinderWriteRead, SetContextMGR, SetMaxThreads, TransactionFlags, TransactionTarget,
};
use dashmap::DashMap;

pub struct Transaction {
    pub code: u32,
    pub payload: PayloadReader,
}

/// Shared binder device state.
#[derive(Debug)]
pub struct BinderDevice {
    fd: Arc<OwnedFd>,
    pub(crate) cookie_counter: AtomicUsize,
    _looper_threads: Vec<std::thread::JoinHandle<()>>,
    pub(crate) owned_ports: Arc<DashMap<BinderObjectId, Arc<BinderObject>>>,
    pub(crate) port_handles: Arc<DashMap<u32, Weak<BinderRef>>>,
    pub(crate) weak_port_handles: Arc<DashMap<u32, Weak<WeakBinderRef>>>,
    // needed for safety
    _backing: BinderBackingMemMap,
}

impl BinderDevice {
    pub unsafe fn get_last_error(&self) -> BinderExtendedError {
        let mut error = BinderExtendedError {
            id: 0,
            command: 0,
            param: 0,
        };
        unsafe { rustix::ioctl::ioctl(self.fd.as_fd(), &mut error) }.unwrap();
        error
    }
    pub fn new(path: impl AsRef<Path>) -> rustix::io::Result<Arc<Self>> {
        let fd = rustix::fs::open(
            path.as_ref(),
            OFlags::CLOEXEC | OFlags::RDONLY,
            Mode::empty(),
        )?;
        Ok(Self::from_fd(fd))
    }
    /// Create a new BinderDevice from an already-open fd.
    pub fn from_fd(fd: impl Into<OwnedFd>) -> Arc<Self> {
        let fd = Arc::new(fd.into());
        let backing = BinderBackingMemMap::new(fd.as_fd(), 1024 * 1024);
        let started = Arc::new(AtomicBool::new(false));
        let dev = Arc::new_cyclic(|weak| {
            let loopers = (0..5)
                .map(|_| {
                    std::thread::spawn({
                        let runtime = tokio::runtime::Handle::current();
                        let fd = fd.clone();
                        let dev = weak.clone();
                        let started = started.clone();
                        move || {
                            let _guard = runtime.enter();
                            // we love busy waiting
                            while !started.load(Ordering::Relaxed) {
                                sleep(Duration::from_millis(1));
                            }
                            drop(started);
                            looper(&runtime, dev, fd);
                        }
                    })
                })
                .collect();
            Self {
                fd,
                cookie_counter: AtomicUsize::new(1),
                _looper_threads: loopers,
                owned_ports: Arc::default(),
                port_handles: Arc::default(),
                weak_port_handles: Arc::default(),
                _backing: backing,
            }
        });
        unsafe {
            rustix::ioctl::ioctl(dev.fd.as_fd(), SetMaxThreads(5)).unwrap();
        }
        started.store(true, Ordering::Relaxed);
        dev
    }

    pub fn context_manager_handle(self: &Arc<Self>) -> Arc<BinderRef> {
        BinderRef::get_context_manager_handle(&self)
    }

    /// Register a handler for incoming transactions and return a binder object.
    ///
    /// When the returned `BinderObject` is dropped, the handler is automatically
    /// unregistered from the device (RAII pattern).
    pub fn register_object<T: TransactionHandler>(
        self: &Arc<Self>,
        handler: T,
    ) -> Arc<BinderObject> {
        let cookie = self.cookie_counter.fetch_add(1, Ordering::Relaxed);

        let handler = Box::new(handler);
        let port = BinderObject::new(cookie, handler, self.clone());

        self.owned_ports.insert(*port.id(), port.clone());

        port
    }

    /// Send a two-way transaction and wait for reply.
    /// WARNING: Only ever call this on a thread where blocking for multiple seconds is acceptable!
    // TODO: make this work with weak handles
    pub fn transact_blocking<'a>(
        self: &Arc<Self>,
        target: &BinderObjectOrRef,
        code: u32,
        data: PayloadBuilder<'_>,
    ) -> Result<(u32, PayloadReader)> {
        let runtime = tokio::runtime::Handle::current();
        match target {
            BinderObjectOrRef::Object(binder_object) => {
                self.self_transact_blocking(binder_object.id(), code, data, &runtime)
            }
            BinderObjectOrRef::WeakObject(weak_binder_object) => {
                self.self_transact_blocking(weak_binder_object.id(), code, data, &runtime)
            }
            BinderObjectOrRef::Ref(binder_ref) => {
                self.remote_transact_blocking(binder_ref.handle(), code, data, &runtime)
            }
            BinderObjectOrRef::WeakRef(weak_binder_ref) => {
                self.remote_transact_blocking(weak_binder_ref.handle(), code, data, &runtime)
            }
        }
    }
    pub async fn set_context_manager(&self, handler: &BinderObject) -> Result<()> {
        let buf = SetContextMGR(handler.get_flat_binder_object());

        let res = unsafe { rustix::ioctl::ioctl(self.fd.as_fd(), buf) };
        if let Err(e) = &res {
            error!("set_context_manager error: {:?}", e);
        }
        // TODO: find more accurate error, also this probably doesn't actually return an error
        res.map_err(|_| Error::PermissionDenied)
    }

    pub(crate) fn remove_binder_port(&self, id: &BinderObjectId) {
        self.owned_ports.remove(&id);
    }
    pub(crate) unsafe fn write_binder_command(&self, data: &[u8]) {
        write_binder_command(&self.fd, data).unwrap()
    }
    pub(crate) unsafe fn write_binder_struct_command<T>(&self, command: BinderCommand, data: &T) {
        write_binder_struct_command(&self.fd, command, data).unwrap()
    }
}
impl BinderDevice {
    fn self_transact_blocking(
        self: &Arc<Self>,
        id: &BinderObjectId,
        code: u32,
        data: PayloadBuilder<'_>,
        runtime: &tokio::runtime::Handle,
    ) -> Result<(u32, PayloadReader)> {
        let obj = self.owned_ports.get(id).ok_or(Error::ObjectNotFound)?;
        let handler = obj.handler();
        let payload = PayloadReader::from_builder(self.clone(), &data);
        let reply = runtime.block_on(handler.handle(Transaction { code, payload }));
        let reply_reader = PayloadReader::from_builder(self.clone(), &reply);
        Ok((code, reply_reader))
    }
    fn remote_transact_blocking<'a>(
        self: &Arc<Self>,
        handle: u32,
        code: u32,
        data: PayloadBuilder<'_>,
        runtime: &tokio::runtime::Handle,
    ) -> Result<(u32, PayloadReader)> {
        let reply = BinderTransactionData {
            target: TransactionTarget { handle },
            cookie: 0,
            code,
            // TODO: actually expose some of these in a reasonable way
            flags: TransactionFlags::ACCEPT_FDS,
            sender_pid: 0,
            sender_euid: 0,
            data_size: data.data_buffer_len() as BinderSizeT,
            offsets_size: (data.offset_buffer_len() * size_of::<usize>()) as BinderSizeT,
            data: crate::sys::BinderTransactionDataPtrs {
                buffer: data.data_buffer_ptr() as _,
                offsets: data.offset_buffer_ptr() as _,
            },
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&BinderCommand::ENTER_LOOPER.as_u32().to_ne_bytes());
        bytes.extend_from_slice(&BinderCommand::TRANSACTION.as_u32().to_ne_bytes());
        bytes.extend_from_slice(unsafe {
            slice::from_raw_parts(&raw const reply as _, size_of_val(&reply))
        });
        bytes.extend_from_slice(&BinderCommand::EXIT_LOOPER.as_u32().to_ne_bytes());
        let mut write_data = Some(bytes.as_slice());
        loop {
            let v = unsafe {
                binder_write_read(&self.fd, write_data.take(), &Arc::downgrade(self), &runtime)
            };
            match v {
                Some(Ok(v)) => break Ok(v),
                Some(Err(WriteReadError::NoDevice)) => {
                    break Err(Error::Shutdown);
                }
                Some(Err(WriteReadError::DeadReply)) => {
                    break Err(Error::DeadReply);
                }
                Some(Err(WriteReadError::ObjectNotFound)) => {
                    break Err(Error::ObjectNotFound);
                }
                Some(Err(WriteReadError::FailedReply)) => {
                    error!("{}", WriteReadError::FailedReply);
                    break Err(Error::Unknown(1));
                }
                Some(Err(WriteReadError::WriteReadIoctlFailed(err))) => {
                    break Err(Error::Binder(err));
                }
                None => continue,
            }
        }
    }
}
#[derive(Debug)]
struct BinderBackingMemMap {
    ptr: *mut c_void,
    len: usize,
}
unsafe impl Send for BinderBackingMemMap {}
unsafe impl Sync for BinderBackingMemMap {}
impl BinderBackingMemMap {
    fn new(fd: impl AsFd, len: usize) -> Self {
        let ptr = unsafe {
            mmap(
                ptr::null_mut(),
                len,
                ProtFlags::READ,
                MapFlags::PRIVATE | MapFlags::NORESERVE,
                fd,
                0,
            )
            .unwrap()
        };
        Self { ptr, len }
    }
}
impl Drop for BinderBackingMemMap {
    fn drop(&mut self) {
        unsafe {
            munmap(self.ptr, self.len).unwrap();
        }
    }
}

unsafe fn write_binder_struct_command<T, Fd: AsFd>(
    fd: impl AsRef<Fd>,
    command: BinderCommand,
    data: &T,
) -> rustix::io::Result<()> {
    let mut bytes = Vec::with_capacity(size_of_val(&command) + size_of_val(data));
    bytes.extend_from_slice(&command.as_u32().to_ne_bytes());
    bytes.extend_from_slice(slice::from_raw_parts(
        data as *const _ as *const u8,
        size_of_val(data),
    ));
    let mut binder_wr = BinderWriteRead {
        write_size: bytes.len() as BinderSizeT,
        write_consumed: 0,
        write_buffer: bytes.as_ptr() as BinderUintptrT,
        read_size: 0,
        read_consumed: 0,
        read_buffer: 0,
    };
    io::retry_on_intr(|| unsafe { rustix::ioctl::ioctl(fd.as_ref(), &mut binder_wr) })
}
unsafe fn write_binder_command<Fd: AsFd>(
    fd: impl AsRef<Fd>,
    data: &[u8],
) -> rustix::io::Result<()> {
    let mut binder_wr = BinderWriteRead {
        write_size: data.len() as BinderSizeT,
        write_consumed: 0,
        write_buffer: data.as_ptr() as BinderUintptrT,
        read_size: 0,
        read_consumed: 0,
        read_buffer: 0,
    };
    io::retry_on_intr(|| unsafe { rustix::ioctl::ioctl(fd.as_ref(), &mut binder_wr) })
}

fn looper(runtime: &tokio::runtime::Handle, device: Weak<BinderDevice>, dev_fd: Arc<OwnedFd>) {
    let mut init_data = Vec::new();
    // init_data.extend_from_slice(&BinderCommand::REGISTER_LOOPER.as_u32().to_ne_bytes());
    init_data.extend_from_slice(&BinderCommand::ENTER_LOOPER.as_u32().to_ne_bytes());
    let mut init_data = Some(init_data.as_slice());
    loop {
        match unsafe { binder_write_read(&dev_fd, init_data.take(), &device, runtime) } {
            Some(Ok(_)) => todo!(),
            Some(Err(WriteReadError::NoDevice)) => {
                break;
            }
            Some(Err(WriteReadError::DeadReply)) => {}
            Some(Err(WriteReadError::ObjectNotFound)) => {}
            Some(Err(WriteReadError::FailedReply)) => {
                error!("{}", WriteReadError::FailedReply);
            }
            Some(Err(WriteReadError::WriteReadIoctlFailed(err))) => {
                error!("WriteRead failed: {err}");
            }
            None => {}
        }
    }
    info!("exiting looper thread :3");
    unsafe {
        write_binder_command(dev_fd, &BinderCommand::EXIT_LOOPER.as_u32().to_ne_bytes()).unwrap();
    }
    // TODO: figure out how the binder thread(not looper) exit call works
}
unsafe fn binder_write_read(
    dev_fd: &Arc<OwnedFd>,
    write_data: Option<&[u8]>,
    device: &Weak<BinderDevice>,
    runtime: &tokio::runtime::Handle,
) -> Option<core::result::Result<(u32, PayloadReader), WriteReadError>> {
    let mut read_data = [0u8; 256];
    let mut binder_wr = BinderWriteRead {
        write_size: write_data.map(|v| v.len()).unwrap_or(0),
        write_consumed: 0,
        write_buffer: write_data
            .map(|v| v.as_ptr() as BinderUintptrT)
            .unwrap_or(0),
        read_size: read_data.len() as BinderSizeT,
        read_consumed: 0,
        read_buffer: read_data.as_mut_ptr() as BinderUintptrT,
    };
    // if write_data.is_some() {
    //     info!(?binder_wr);
    // }
    // info!(v = write_data.is_some());
    let res = io::retry_on_intr(|| unsafe { rustix::ioctl::ioctl(&dev_fd, &mut binder_wr) });
    if let Err(err) = res {
        error!("binder write_read call failed: {err}");
        return Some(Err(WriteReadError::WriteReadIoctlFailed(err)));
    }
    let Some(device) = device.upgrade() else {
        return Some(Err(WriteReadError::NoDevice));
    };
    let mut consumed = 0;
    while consumed != binder_wr.read_consumed {
        let read_slice = &read_data[consumed..binder_wr.read_consumed as usize];
        let header = size_of::<u32>();
        let ret = BinderReturn::from_u32(unsafe {
            read_from_slice(&read_slice[..header], &mut consumed)
        });
        match ret {
            BinderReturn::ERROR => {
                let err = unsafe { read_from_slice::<i32>(&read_slice[header..], &mut consumed) };
                error!("received binder error: {err}");
            }
            BinderReturn::OK => {
                debug!("received ok");
            }
            BinderReturn::TRANSACTION_SEC_CTX | BinderReturn::TRANSACTION => {
                let (sec_ctx, transaction) = if ret == BinderReturn::TRANSACTION_SEC_CTX {
                    let v = unsafe {
                        read_from_slice::<BinderTransactionDataSecCtx>(
                            &read_slice[header..],
                            &mut consumed,
                        )
                    };
                    (Some(v.sec_ctx), v.transaction_data)
                } else {
                    (None, unsafe {
                        read_from_slice::<BinderTransactionData>(
                            &read_slice[header..],
                            &mut consumed,
                        )
                    })
                };
                // Safety: incomming transactions will always use the local identifier
                let target = BinderObjectId::from_raw(
                    unsafe { transaction.target.binder },
                    transaction.cookie,
                );
                let Some(handler) = device.owned_ports.get(&target) else {
                    warn!("unable to find handler for: {target:x?}");
                    return Some(Err(WriteReadError::ObjectNotFound));
                };
                let payload_reader = unsafe {
                    PayloadReader::from_kernel_raw(
                        device.clone(),
                        transaction.data.buffer as *const u8,
                        transaction.data_size,
                        transaction.data.offsets as *const usize,
                        transaction.offsets_size / size_of::<usize>(),
                    )
                };
                if transaction.flags.contains(TransactionFlags::ONE_WAY) {
                    runtime.block_on(handler.handler().handle_one_way(Transaction {
                        code: transaction.code,
                        payload: payload_reader,
                    }));
                } else {
                    let reply_data = runtime.block_on(handler.handler().handle(Transaction {
                        code: transaction.code,
                        payload: payload_reader,
                    }));

                    let reply = BinderTransactionData {
                        // unused in reply
                        target: TransactionTarget { binder: 0 },
                        // unused in reply
                        cookie: 0,
                        code: transaction.code,
                        flags: transaction.flags,
                        sender_pid: rustix::process::getpid().as_raw_pid(),
                        sender_euid: rustix::process::getuid().as_raw(),
                        data_size: reply_data.data_buffer_len() as BinderSizeT,
                        offsets_size: (reply_data.offset_buffer_len() * size_of::<usize>())
                            as BinderSizeT,
                        data: crate::sys::BinderTransactionDataPtrs {
                            buffer: reply_data.data_buffer_ptr() as _,
                            offsets: reply_data.offset_buffer_ptr() as _,
                        },
                    };
                    let mut bytes = Vec::new();
                    bytes.extend_from_slice(&BinderCommand::REPLY.as_u32().to_ne_bytes());
                    bytes.extend_from_slice(slice::from_raw_parts(
                        &raw const reply as _,
                        size_of_val(&reply),
                    ));
                    write_binder_command(dev_fd, &bytes).unwrap();
                    drop(reply_data);
                }
            }
            BinderReturn::REPLY => {
                let reply = unsafe {
                    read_from_slice::<BinderTransactionData>(&read_slice[header..], &mut consumed)
                };
                trace!("received reply");
                return Some(Ok((reply.code, unsafe {
                    PayloadReader::from_kernel_raw(
                        device.clone(),
                        reply.data.buffer as *const u8,
                        reply.data_size,
                        reply.data.offsets as *const usize,
                        reply.offsets_size / size_of::<usize>(),
                    )
                })));
            }
            // TODO: implement?
            BinderReturn::ACQUIRE_RESULT => {
                let v = unsafe { read_from_slice::<i32>(&read_slice[header..], &mut consumed) };
                debug!("attempted strong ref increase result?");
            }
            BinderReturn::DEAD_REPLY => {
                return Some(Err(WriteReadError::DeadReply));
            }
            // TODO: implement
            BinderReturn::TRANSACTION_COMPLETE => {
                trace!("transaction complete");
            }
            BinderReturn::INCREFS => {
                let v = unsafe {
                    read_from_slice::<BinderPtrCookie>(&read_slice[header..], &mut consumed)
                };
                // TODO: actually track maybe?
                _ = write_binder_struct_command(dev_fd, BinderCommand::INCREFS_DONE, &v)
                    .inspect_err(|err| error!("failed to send INCREFS_DONE: {err}"));
            }
            BinderReturn::ACQUIRE => {
                let v = unsafe {
                    read_from_slice::<BinderPtrCookie>(&read_slice[header..], &mut consumed)
                };
                // TODO: actually track
                _ = write_binder_struct_command(dev_fd, BinderCommand::ACQUIRE_DONE, &v)
                    .inspect_err(|err| error!("failed to send ACQUIRE_DONE: {err}"));
            }
            BinderReturn::RELEASE => {
                let v = unsafe {
                    read_from_slice::<BinderPtrCookie>(&read_slice[header..], &mut consumed)
                };
                // TODO: actually track
                debug!("strong ref decrease");
            }
            BinderReturn::DECREFS => {
                let v = unsafe {
                    read_from_slice::<BinderPtrCookie>(&read_slice[header..], &mut consumed)
                };
                // TODO: actually track maybe?
                debug!("weak ref decrease");
            }
            BinderReturn::ATTEMPT_ACQUIRE => {
                let _v = unsafe {
                    read_from_slice::<BinderPtrCookie>(&read_slice[header..], &mut consumed)
                };
                debug!("attempt strong ref increase, should be unused i think?");
            }
            BinderReturn::NOOP => {
                trace!("noop?");
            }
            BinderReturn::SPAWN_LOOPER => {
                let device = Arc::downgrade(&device);
                let dev_fd = dev_fd.clone();
                let runtime = runtime.clone();
                std::thread::spawn(move || looper(&runtime, device, dev_fd));
            }
            BinderReturn::FINISHED => {
                debug!("finished?");
            }
            BinderReturn::DEAD_BINDER => {
                let v = unsafe {
                    read_from_slice::<BinderUintptrT>(&read_slice[header..], &mut consumed)
                };
                // TODO: impl
                debug!("dead binder?");
            }
            BinderReturn::CLEAR_DEATH_NOTIFICATION_DONE => {
                let v = unsafe {
                    read_from_slice::<BinderUintptrT>(&read_slice[header..], &mut consumed)
                };
                // TODO: impl?
                debug!("clear death notif");
            }
            BinderReturn::FAILED_REPLY => {
                warn!("failed reply: {:?}", unsafe { device.get_last_error() });
                return Some(Err(WriteReadError::FailedReply));
            }
            BinderReturn::FROZEN_REPLY => {
                debug!("frozen reply");
            }
            BinderReturn::ONEWAY_SPAM_SUSPECT => {
                debug!("oneway spam suspect");
            }
            BinderReturn::TRANSACTION_PENDING_FROZEN => {
                debug!("transaction pending frozen")
            }
            BinderReturn::FROZEN_BINDER => {
                let v = unsafe {
                    read_from_slice::<BinderFrozenStateInfo>(&read_slice[header..], &mut consumed)
                };
                debug!("frozen object")
            }
            BinderReturn::CLEAR_FREEZE_NOTIFICATION_DONE => {
                let v = unsafe {
                    read_from_slice::<BinderUintptrT>(&read_slice[header..], &mut consumed)
                };
                debug!("cleared freeze notif")
            }
            msg_type => {
                error!("unknown binder message: {msg_type:?}");
            }
        }
    }
    None
}
#[derive(Error, Debug)]
enum WriteReadError {
    #[error("BinderObject for transaction target not found")]
    ObjectNotFound,
    #[error("Dead Reply")]
    DeadReply,
    #[error("Reply Failed")]
    FailedReply,
    #[error("No device")]
    NoDevice,
    #[error("WriteRead failed: {0}")]
    WriteReadIoctlFailed(Errno),
}
unsafe fn read_from_slice<T>(slice: &[u8], consumed: &mut usize) -> T {
    assert!(slice.len() >= size_of::<T>());
    *consumed += size_of::<T>();
    ptr::read_unaligned(slice.as_ptr().cast())
}

#[async_trait::async_trait]
pub(crate) trait DynTransactionHandler: Send + Sync + 'static {
    async fn handle(&self, transaction: Transaction) -> PayloadBuilder;
    async fn handle_one_way(&self, transaction: Transaction);
}

pub trait TransactionHandler: Send + Sync + 'static {
    fn handle(
        &self,
        transaction: Transaction,
    ) -> impl std::future::Future<Output = PayloadBuilder<'_>> + std::marker::Send;

    fn handle_one_way(
        &self,
        transaction: Transaction,
    ) -> impl std::future::Future<Output = ()> + std::marker::Send;
}

#[async_trait::async_trait]
impl<T: TransactionHandler> DynTransactionHandler for T {
    async fn handle(&self, transaction: Transaction) -> PayloadBuilder {
        <Self as TransactionHandler>::handle(&self, transaction).await
    }

    async fn handle_one_way(&self, transaction: Transaction) {
        <Self as TransactionHandler>::handle_one_way(&self, transaction).await
    }
}
