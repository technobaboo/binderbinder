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
use std::any::Any;
use std::ffi::c_void;
use std::fmt::Debug;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::thread::sleep;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::Notify;
use tracing::{debug, error, info, trace, warn};

use crate::binder_object::{
    BinderObject, BinderObjectId, BinderRef, ContextManagerBinderRef, TransactionTarget,
    WeakBinderRef,
};
use crate::error::{Error, Result};
use crate::payload::{PayloadBuilder, PayloadReader};
use crate::sys::{
    self, BinderCommand, BinderExtendedError, BinderFrozenStateInfo, BinderPtrCookie, BinderReturn,
    BinderSizeT, BinderTransactionData, BinderTransactionDataSecCtx, BinderUintptrT,
    BinderWriteRead, FlatBinderObject, SetContextMGR, SetMaxThreads, TransactionFlags,
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
    pub(crate) object_id_counter: AtomicUsize,
    pub(crate) death_counter: AtomicUsize,
    _looper_threads: Vec<std::thread::JoinHandle<()>>,
    pub(crate) objects: Arc<DashMap<BinderObjectId, Arc<dyn DynBinderObject>>>,
    pub(crate) refs: Arc<DashMap<u32, Weak<BinderRef>>>,
    pub(crate) weak_refs: Arc<DashMap<u32, Weak<WeakBinderRef>>>,
    pub(crate) death_notifications: Arc<DashMap<usize, Arc<Notify>>>,
    ctx_manager: ContextManagerBinderRef,
    // needed for safety
    _backing: BinderBackingMemMap,
}

impl BinderDevice {
    /// # Safety
    /// Assumes there is an error to be returned.
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
                object_id_counter: AtomicUsize::new(1),
                death_counter: AtomicUsize::new(1),
                _looper_threads: loopers,
                objects: Arc::default(),
                refs: Arc::default(),
                weak_refs: Arc::default(),
                _backing: backing,
                death_notifications: Arc::default(),
                ctx_manager: ContextManagerBinderRef(AtomicUsize::new(0)),
            }
        });
        unsafe {
            rustix::ioctl::ioctl(dev.fd.as_fd(), SetMaxThreads(5)).unwrap();
        }
        started.store(true, Ordering::Relaxed);
        dev
    }

    /// Register a handler for incoming transactions and return a binder object.
    ///
    /// When the returned `BinderObject` is dropped, the handler is automatically
    /// unregistered from the device (RAII pattern).
    pub fn register_object<T: TransactionHandler>(
        self: &Arc<Self>,
        handler: T,
    ) -> Arc<BinderObject<T>> {
        let cookie = self.object_id_counter.fetch_add(1, Ordering::Relaxed);

        let port = BinderObject::new(cookie, handler, self.clone());

        self.objects.insert(*port.id(), port.clone());

        port
    }

    /// Send a two-way transaction and wait for reply.
    /// WARNING: Only ever call this on a thread where blocking for multiple seconds is acceptable!
    // TODO: make this work with weak handles
    pub fn transact_blocking(
        self: &Arc<Self>,
        target: &dyn TransactionTarget,
        code: u32,
        data: PayloadBuilder<'_>,
    ) -> Result<(u32, PayloadReader)> {
        let runtime = tokio::runtime::Handle::current();
        match target.get_transaction_target_handle() {
            crate::binder_object::TransactionTargetHandle::Local(id) => {
                self.self_transact_blocking(&id, code, data, &runtime)
            }
            crate::binder_object::TransactionTargetHandle::Remote(handle) => {
                self.remote_transact_blocking(handle, code, data, &runtime)
            }
        }
    }
    pub async fn set_context_manager<T: TransactionHandler>(
        &self,
        handler: &BinderObject<T>,
    ) -> Result<()> {
        let buf = SetContextMGR(handler.get_flat_binder_object());
        // if we ever change the BinderObjectId to have a non 0 cookie, this breaks
        self.ctx_manager.0.store(handler.id().id, Ordering::Relaxed);

        let res = unsafe { rustix::ioctl::ioctl(self.fd.as_fd(), buf) };
        if let Err(e) = &res {
            error!("set_context_manager error: {:?}", e);
        }
        // TODO: find more accurate error, also this probably doesn't actually return an error
        res.map_err(|_| Error::PermissionDenied)
    }
    pub fn context_manager(&self) -> &ContextManagerBinderRef {
        &self.ctx_manager
    }

    pub(crate) fn remove_binder_object(&self, id: &BinderObjectId) {
        self.objects.remove(id);
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
        let handler = self.objects.get(id).ok_or(Error::ObjectNotFound)?;
        let payload = PayloadReader::from_builder(self.clone(), &data);
        let reply = runtime.block_on(handler.handle(Transaction { code, payload }));
        let reply_reader = PayloadReader::from_builder(self.clone(), &reply);
        Ok((code, reply_reader))
    }
    fn remote_transact_blocking(
        self: &Arc<Self>,
        handle: u32,
        code: u32,
        data: PayloadBuilder<'_>,
        runtime: &tokio::runtime::Handle,
    ) -> Result<(u32, PayloadReader)> {
        let reply = BinderTransactionData {
            target: sys::TransactionTarget { handle },
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
                binder_write_read(&self.fd, write_data.take(), &Arc::downgrade(self), runtime)
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
    let res = io::retry_on_intr(|| unsafe { rustix::ioctl::ioctl(dev_fd, &mut binder_wr) });
    if let Err(err) = res {
        error!("binder write_read call failed: {err}");
        return Some(Err(WriteReadError::WriteReadIoctlFailed(err)));
    }
    let Some(device) = device.upgrade() else {
        return Some(Err(WriteReadError::NoDevice));
    };
    let mut consumed = 0;
    while consumed != binder_wr.read_consumed {
        let read_slice = &read_data[consumed..binder_wr.read_consumed];
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
                let (_sec_ctx, transaction) = if ret == BinderReturn::TRANSACTION_SEC_CTX {
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
                let Some(handler) = device.objects.get(&target) else {
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
                    runtime.block_on(handler.handle_one_way(Transaction {
                        code: transaction.code,
                        payload: payload_reader,
                    }));
                } else {
                    let reply_data = runtime.block_on(handler.handle(Transaction {
                        code: transaction.code,
                        payload: payload_reader,
                    }));

                    let reply = BinderTransactionData {
                        // unused in reply
                        target: sys::TransactionTarget { binder: 0 },
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
                let _v = unsafe { read_from_slice::<i32>(&read_slice[header..], &mut consumed) };
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
                let _v = unsafe {
                    read_from_slice::<BinderPtrCookie>(&read_slice[header..], &mut consumed)
                };
                // TODO: actually track
                debug!("strong ref decrease");
            }
            BinderReturn::DECREFS => {
                let _v = unsafe {
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
                if let Some((_, notify)) = device.death_notifications.remove(&v) {
                    notify.notify_waiters();
                } else {
                    warn!("got DeadBinder without having internal death_notification registered for it");
                }
                _ = write_binder_struct_command(dev_fd, BinderCommand::DEAD_BINDER_DONE, &v);
            }
            BinderReturn::CLEAR_DEATH_NOTIFICATION_DONE => {
                let _v = unsafe {
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
                let _v = unsafe {
                    read_from_slice::<BinderFrozenStateInfo>(&read_slice[header..], &mut consumed)
                };
                debug!("frozen object")
            }
            BinderReturn::CLEAR_FREEZE_NOTIFICATION_DONE => {
                let _v = unsafe {
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
pub(crate) trait DynBinderObject:
    Any + TransactionTarget + Debug + Send + Sync + 'static
{
    async fn handle(&self, transaction: Transaction) -> PayloadBuilder;
    async fn handle_one_way(&self, transaction: Transaction);
    fn get_flat_binder_object(&self) -> FlatBinderObject;
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
