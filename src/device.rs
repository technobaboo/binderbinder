use crate::binder_object::{
    BinderObject, BinderObjectId, BinderRef, ContextManagerBinderRef, TransactionTarget,
    WeakBinderRef,
};
use crate::error::{Error, Result};
use crate::payload::{PayloadBuilder, PayloadReader};
use crate::sys::{
    self, BinderCommand, BinderExtendedError, BinderFrozenStateInfo, BinderObjectHeader,
    BinderPtrCookie, BinderReturn, BinderSizeT, BinderTransactionData, BinderTransactionDataSecCtx,
    BinderType, BinderUintptrT, BinderWriteRead, FlatBinderObject, SetContextMGR, SetMaxThreads,
    TransactionFlags,
};
use core::slice;
use dashmap::DashMap;
use rustix::fs::{Mode, OFlags};
use rustix::io::{self, Errno};
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use rustix::process::{self, RawPid, RawUid};
use std::any::{Any, type_name_of_val};
use std::ffi::c_void;
use std::fmt::Debug;
use std::future::Future;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::pin::Pin;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::thread::sleep;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info, instrument, trace, trace_span, warn};

pub struct Transaction {
    pub code: u32,
    pub payload: PayloadReader,
    pub sender_pid: RawPid,
    pub sender_euid: RawUid,
}

/// `local`/`remote`/`pending` always move together as one atomic unit — see
/// the comment on [`ObjectRefState::counts`] for why that matters.
#[derive(Debug, Default, Clone, Copy)]
struct RefCounts {
    local: u32,
    remote: u32,
    /// Number of in-flight sends of this object that haven't yet been
    /// confirmed (by their real BR_ACQUIRE landing) or rolled back (because
    /// the send failed). Guards against reporting a premature zero between
    /// a sender's transient local ref dropping and that confirmation
    /// arriving. See [`ObjectRefState::mark_pending_remote`].
    ///
    /// This has to be a *count*, not a single flag: the same object can be
    /// embedded in more than one concurrent outgoing send before either is
    /// confirmed (e.g. handed to two different clients at once). A bool
    /// can't tell those apart — one send failing and rolling back would
    /// clear the *other* send's still-legitimate guard too, letting
    /// `decrease_local` report a zero while that other send is still
    /// genuinely in flight (the object gets torn down before the transfer
    /// that's about to reference it even lands).
    pending: u32,
}
impl RefCounts {
    fn total(&self) -> u32 {
        self.local + self.remote + self.pending
    }
}

#[derive(Debug)]
pub(crate) struct ObjectRefState {
    obj_id: BinderObjectId,
    /// `local`, `remote`, and `pending` used to be three independent
    /// atomics, each mutating method reading the *other* two via plain
    /// `.load()`s to decide what to publish. That's racy: two concurrent
    /// updates (e.g. the owner dropping its local ref right as the remote
    /// side's BR_RELEASE lands) can each read a stale value for the field
    /// the other just changed, so each one computes and publishes a
    /// nonzero total — and since nothing else will ever mutate this
    /// object's counts again once both sides have genuinely let go, the
    /// real zero is never observed and the object sticks around forever.
    /// A single mutex serializes every mutation into one consistent
    /// snapshot, so the *last* transition to true zero is always the one
    /// that gets published.
    counts: std::sync::Mutex<RefCounts>,
    /// `local + remote`, plus one extra unit while `pending` is set. Zero
    /// means genuinely no strong refs outstanding.
    ///
    /// This used to be a pair of `Notify`s (one for hit-zero, one for
    /// not-zero), fired as edge events via `notify_waiters()`. That's
    /// fundamentally the wrong shape for "wait until this state holds":
    /// `notify_waiters()` only wakes tasks *already polling* at the instant
    /// it's called — a task that subscribes after the fact (as
    /// `strong_refs_hit_zero()` used to, since it only called `.notified()`
    /// on first poll instead of at construction) waits forever for an edge
    /// that already happened and won't happen again. A `watch` is
    /// level-triggered instead: `Receiver::wait_for` always checks the
    /// *current* value first, so subscribing late still observes the right
    /// outcome instead of hanging.
    strong_count: watch::Sender<u32>,
}
impl ObjectRefState {
    pub(crate) fn new(obj_id: BinderObjectId) -> Self {
        Self {
            obj_id,
            counts: std::sync::Mutex::new(RefCounts::default()),
            strong_count: watch::Sender::new(0),
        }
    }
    pub(crate) fn subscribe(&self) -> watch::Receiver<u32> {
        self.strong_count.subscribe()
    }
    /// Applies `f` to the current counts under the lock and publishes the
    /// resulting total, returning the post-update snapshot.
    ///
    /// The publish (`send_replace`) has to happen *while still holding the
    /// lock*, not after releasing it. If it happened after, two concurrent
    /// callers could lock/mutate in order A-then-B (B's state is the true
    /// latest) but get preempted and reach their own `send_replace` calls
    /// in order B-then-A — A's older snapshot would overwrite B's correct,
    /// more recent one, and since nothing else will ever mutate this
    /// object's counts again once it's genuinely done, that stale
    /// published value sticks forever. Publishing inside the lock forces
    /// publish order to match mutation order.
    fn update(&self, f: impl FnOnce(&mut RefCounts)) -> RefCounts {
        let mut counts = self.counts.lock().unwrap();
        f(&mut counts);
        let snapshot = *counts;
        tracing::debug!(?self.obj_id, total = snapshot.total(), "publishing strong count");
        self.strong_count.send_replace(snapshot.total());
        snapshot
    }
    pub(crate) fn increase_local(&self) {
        let snapshot = self.update(|c| c.local += 1);
        tracing::debug!(?self.obj_id, new_count = snapshot.local, "increasing local strong ref");
    }
    pub(crate) fn decrease_local(&self) {
        let snapshot = self.update(|c| c.local -= 1);
        tracing::debug!(?self.obj_id, new_count = snapshot.local, "decreasing local strong ref");
    }
    pub(crate) fn increase_remote(&self) {
        let snapshot = self.update(|c| {
            c.remote += 1;
            // A *full* reset, not a decrement-by-one: the kernel only ever
            // queues a BR_ACQUIRE for the actual 0→1 transition, so if
            // several concurrent sends all raced `mark_pending_remote`
            // while remote was still 0, only one of them "wins" and gets
            // its own ACQUIRE — the rest never individually get one to
            // consume their credit. But that's fine: once remote is
            // confirmed established, none of those racing credits are
            // needed anymore regardless of how many there were, since
            // remote alone now keeps the total nonzero. Decrementing by
            // just one here would leave the losing racers' credits stuck
            // forever.
            c.pending = 0;
        });
        tracing::debug!(?self.obj_id, new_count = snapshot.remote, "increasing remote strong ref");
    }
    pub(crate) fn decrease_remote(&self) {
        let snapshot = self.update(|c| c.remote -= 1);
        tracing::debug!(?self.obj_id, new_count = snapshot.remote, "decreasing remote strong ref");
    }
    /// About to hand this object to a remote process in a transaction: guards against
    /// `decrease_local` reporting a premature zero between the sender's
    /// transient local ref dropping and the matching BR_ACQUIRE coming back.
    ///
    /// Only guards while `remote == 0`: once the object already has an
    /// established remote ref, `decrease_local` can't zero the total on its
    /// own (remote alone keeps it nonzero), and re-sending an
    /// already-acquired object to another holder doesn't generate a fresh
    /// BR_ACQUIRE from the kernel (it only queues one on the first 0→1
    /// transition) — so a guard taken out here would never get consumed by
    /// `increase_remote` and would sit stuck forever.
    pub(crate) fn mark_pending_remote(&self) {
        self.update(|c| {
            if c.remote == 0 {
                c.pending += 1;
            }
        });
    }
    /// Roll back this send's own `mark_pending_remote` after it failed to
    /// complete (write failed, dead reply, peer died mid-flight, ...), so
    /// the BR_ACQUIRE that would normally clear it is never going to
    /// arrive. Only decrements this send's own unit of guard — any other
    /// concurrent in-flight send of the same object keeps its protection.
    /// Without this at all, `pending` would stay stuck above zero forever
    /// and the object could never be reported as genuinely unreferenced
    /// again.
    pub(crate) fn clear_pending_remote(&self) {
        self.update(|c| c.pending = c.pending.saturating_sub(1));
    }
}

/// Shared binder device state.
pub struct BinderDevice {
    fd: Arc<OwnedFd>,
    pub(crate) object_id_counter: AtomicUsize,
    pub(crate) death_counter: AtomicUsize,
    _looper_threads: Vec<std::thread::JoinHandle<()>>,
    pub(crate) objects: DashMap<BinderObjectId, Arc<dyn ErasedTransactionHandler>>,
    pub(crate) object_refcounts: DashMap<BinderObjectId, ObjectRefState>,
    pub(crate) retained_services: DashMap<BinderObjectId, Box<dyn Any + Send + Sync>>,
    pub(crate) refs: DashMap<u32, Weak<BinderRef>>,
    pub(crate) weak_refs: DashMap<u32, Weak<WeakBinderRef>>,
    pub(crate) death_notifications: DashMap<usize, CancellationToken>,
    ctx_manager: ContextManagerBinderRef,
    // needed for safety
    _backing: BinderBackingMemMap,
}

impl Debug for BinderDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BinderDevice")
            // TODO: get rid of this alloc
            .field(
                "objects",
                &self.objects.iter().map(|o| *o.key()).collect::<Vec<_>>(),
            )
            .field(
                "retained_services",
                &self
                    .retained_services
                    .iter()
                    .map(|o| *o.key())
                    .collect::<Vec<_>>(),
            )
            .field(
                "refs",
                &self.refs.iter().map(|o| *o.key()).collect::<Vec<_>>(),
            )
            .field(
                "weak_refs",
                &self.weak_refs.iter().map(|o| *o.key()).collect::<Vec<_>>(),
            )
            .finish()
    }
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
        let fd = rustix::fs::open(path.as_ref(), OFlags::CLOEXEC | OFlags::RDWR, Mode::empty())?;
        Ok(Self::from_fd(fd, 5))
    }
    /// Create a new BinderDevice from an already-open fd.
    pub fn from_fd(fd: impl Into<OwnedFd>, looper_count: usize) -> Arc<Self> {
        let fd = Arc::new(fd.into());
        let backing = BinderBackingMemMap::new(fd.as_fd(), 1024 * 1024);
        let started = Arc::new(AtomicBool::new(false));
        let dev = Arc::new_cyclic(|weak| {
            let loopers = (0..looper_count)
                .filter_map(|i| {
                    std::thread::Builder::new()
                        .name(format!("Binder looper {i}"))
                        .spawn({
                            let runtime = tokio::runtime::Handle::current();
                            let fd = fd.clone();
                            let dev = weak.clone();
                            let started = started.clone();
                            move || {
                                // we love busy waiting
                                while !started.load(Ordering::Relaxed) {
                                    sleep(Duration::from_millis(1));
                                }
                                drop(started);
                                looper(&runtime, dev, fd, false);
                            }
                        })
                        .ok()
                })
                .collect();
            Self {
                fd,
                object_id_counter: AtomicUsize::new(1),
                death_counter: AtomicUsize::new(1),
                _looper_threads: loopers,
                objects: DashMap::default(),
                object_refcounts: DashMap::default(),
                retained_services: DashMap::default(),
                refs: DashMap::default(),
                weak_refs: DashMap::default(),
                _backing: backing,
                death_notifications: DashMap::default(),
                ctx_manager: ContextManagerBinderRef(AtomicUsize::new(0)),
            }
        });
        unsafe {
            rustix::ioctl::ioctl(dev.fd.as_fd(), SetMaxThreads(5)).unwrap();
        }
        started.store(true, Ordering::Relaxed);
        dev
    }

    /// Register a handler for incoming transactions and return a capability guard.
    ///
    /// When the returned `BinderObject` is dropped, the handler is automatically
    /// unregistered from the device (RAII pattern).
    pub fn register_object<T: TransactionHandler>(
        self: &Arc<Self>,
        handler: impl Into<Arc<T>>,
    ) -> BinderObject<T> {
        let id = self.object_id_counter.fetch_add(1, Ordering::Relaxed);
        let id = BinderObjectId { id, cookie: 0 };
        let handler = handler.into();

        self.objects
            .insert(id, handler.clone() as Arc<dyn ErasedTransactionHandler>);
        self.object_refcounts.insert(id, ObjectRefState::new(id));

        BinderObject {
            device: self.clone(),
            id,
            handler,
        }
    }

    /// Get the handler for a given object ID (for payload decoding / downcasting).
    pub(crate) fn get_handler(
        &self,
        id: &BinderObjectId,
    ) -> Option<Arc<dyn ErasedTransactionHandler>> {
        self.objects.get(id).map(|v| v.value().clone())
    }

    /// Send a two-way transaction and wait for reply.
    /// WARNING: Only ever call this on a thread where blocking for multiple seconds is acceptable!
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
    pub fn transact_one_way(
        self: &Arc<Self>,
        target: &dyn TransactionTarget,
        code: u32,
        data: PayloadBuilder<'_>,
    ) -> Result<()> {
        let runtime = tokio::runtime::Handle::current();
        match target.get_transaction_target_handle() {
            crate::binder_object::TransactionTargetHandle::Local(id) => {
                self.self_transact_one_way(&id, code, data)
            }
            crate::binder_object::TransactionTargetHandle::Remote(handle) => {
                self.remote_transact_one_way(handle, code, data, &runtime)
            }
        }
    }
    pub async fn set_context_manager<T: TransactionHandler>(
        &self,
        obj: &BinderObject<T>,
    ) -> Result<()> {
        let flat = FlatBinderObject {
            hdr: crate::sys::BinderObjectHeader {
                type_: crate::sys::BinderType::BINDER,
            },
            flags: crate::sys::FlatBinderFlags::ACCEPTS_FDS,
            data: crate::sys::FlatBinderObjectData {
                binder: obj.id().id,
            },
            cookie: obj.id().cookie,
        };
        let buf = SetContextMGR(flat);
        // if we ever change the BinderObjectId to have a non 0 cookie, this breaks
        self.ctx_manager.0.store(obj.id().id, Ordering::Relaxed);

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
        self.object_refcounts.remove(id);
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
        let _guard = trace_span!("Local blocking transaction").entered();
        let handler = self.objects.get(id).ok_or(Error::ObjectNotFound)?.clone();
        let payload = PayloadReader::from_builder(self.clone(), &data);
        let reply = runtime.block_on(handler.handle(Transaction {
            code,
            payload,
            sender_pid: process::getpid().as_raw_pid(),
            sender_euid: process::geteuid().as_raw(),
        }));
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
        let transaction = BinderTransactionData {
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
        unsafe { mark_objects_as_pending_remote(self, &transaction) };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&BinderCommand::ENTER_LOOPER.as_u32().to_ne_bytes());
        bytes.extend_from_slice(&BinderCommand::TRANSACTION.as_u32().to_ne_bytes());
        bytes.extend_from_slice(unsafe {
            slice::from_raw_parts(&raw const transaction as _, size_of_val(&transaction))
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
                    unsafe { unmark_objects_as_pending_remote(self, &transaction) };
                    break Err(Error::Shutdown);
                }
                Some(Err(WriteReadError::DeadReply)) => {
                    unsafe { unmark_objects_as_pending_remote(self, &transaction) };
                    break Err(Error::DeadReply);
                }
                Some(Err(WriteReadError::ObjectNotFound)) => {
                    unsafe { unmark_objects_as_pending_remote(self, &transaction) };
                    break Err(Error::ObjectNotFound);
                }
                Some(Err(WriteReadError::FailedReply)) => {
                    error!("remote twoway {}", WriteReadError::FailedReply);
                    unsafe { unmark_objects_as_pending_remote(self, &transaction) };
                    break Err(Error::Unknown(1));
                }
                Some(Err(WriteReadError::FrozenReply)) => {
                    unsafe { unmark_objects_as_pending_remote(self, &transaction) };
                    break Err(Error::FrozenReply);
                }
                Some(Err(WriteReadError::AsyncBufferFull)) => {
                    sleep(Duration::from_millis(1));
                }
                Some(Err(WriteReadError::WriteReadIoctlFailed(err))) => {
                    unsafe { unmark_objects_as_pending_remote(self, &transaction) };
                    break Err(Error::Binder(err));
                }
                None => continue,
            }
        }
    }
    fn self_transact_one_way(
        self: &Arc<Self>,
        id: &BinderObjectId,
        code: u32,
        data: PayloadBuilder<'_>,
    ) -> Result<()> {
        let handler = self.objects.get(id).ok_or(Error::ObjectNotFound)?.clone();
        let payload = PayloadReader::from_builder(self.clone(), &data);
        let handler_type = handler.type_name();
        tokio::spawn(
            async move {
                handler
                    .handle_one_way(Transaction {
                        code,
                        payload,
                        sender_pid: process::getpid().as_raw_pid(),
                        sender_euid: process::geteuid().as_raw(),
                    })
                    .instrument(trace_span!("Local oneway transaction"))
                    .await
            }
            .instrument(trace_span!(
                "self transact_one_way",
                handler_type,
                ?id,
                code
            )),
        );
        Ok(())
    }
    #[instrument(
        name = "Remote oneway transaction",
        level = "trace",
        skip(self, data, runtime)
    )]
    fn remote_transact_one_way(
        self: &Arc<Self>,
        handle: u32,
        code: u32,
        data: PayloadBuilder<'_>,
        runtime: &tokio::runtime::Handle,
    ) -> Result<()> {
        let transaction = BinderTransactionData {
            target: sys::TransactionTarget { handle },
            cookie: 0,
            code,
            // TODO: actually expose some of these in a reasonable way
            flags: TransactionFlags::ACCEPT_FDS | TransactionFlags::ONE_WAY,
            sender_pid: 0,
            sender_euid: 0,
            data_size: data.data_buffer_len() as BinderSizeT,
            offsets_size: (data.offset_buffer_len() * size_of::<usize>()) as BinderSizeT,
            data: crate::sys::BinderTransactionDataPtrs {
                buffer: data.data_buffer_ptr() as _,
                offsets: data.offset_buffer_ptr() as _,
            },
        };
        unsafe { mark_objects_as_pending_remote(self, &transaction) };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&BinderCommand::ENTER_LOOPER.as_u32().to_ne_bytes());
        bytes.extend_from_slice(&BinderCommand::TRANSACTION.as_u32().to_ne_bytes());
        bytes.extend_from_slice(unsafe {
            slice::from_raw_parts(&raw const transaction as _, size_of_val(&transaction))
        });
        bytes.extend_from_slice(&BinderCommand::EXIT_LOOPER.as_u32().to_ne_bytes());
        let weak = Arc::downgrade(self);
        loop {
            let v = unsafe { binder_write_read(&self.fd, Some(bytes.as_slice()), &weak, runtime) };
            match v {
                Some(Err(WriteReadError::AsyncBufferFull)) => {
                    std::thread::yield_now();
                    continue;
                }
                Some(Err(WriteReadError::NoDevice)) => {
                    unsafe { unmark_objects_as_pending_remote(self, &transaction) };
                    return Err(Error::Shutdown);
                }
                Some(Err(WriteReadError::DeadReply)) => {
                    unsafe { unmark_objects_as_pending_remote(self, &transaction) };
                    return Err(Error::DeadReply);
                }
                Some(Err(WriteReadError::FrozenReply)) => {
                    unsafe { unmark_objects_as_pending_remote(self, &transaction) };
                    return Err(Error::FrozenReply);
                }
                Some(Err(WriteReadError::ObjectNotFound)) => {
                    unsafe { unmark_objects_as_pending_remote(self, &transaction) };
                    return Err(Error::ObjectNotFound);
                }
                Some(Err(WriteReadError::FailedReply)) => {
                    error!("remote transact oneway {}", WriteReadError::FailedReply);
                    unsafe { unmark_objects_as_pending_remote(self, &transaction) };
                    return Err(Error::Unknown(1));
                }
                Some(Err(WriteReadError::WriteReadIoctlFailed(err))) => {
                    unsafe { unmark_objects_as_pending_remote(self, &transaction) };
                    return Err(Error::Binder(err));
                }
                _ => return Ok(()),
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

/// Walks the flat binder objects embedded in an outgoing transaction's payload.
unsafe fn for_each_embedded_binder_object(
    transaction: &BinderTransactionData,
    mut f: impl FnMut(BinderObjectId),
) {
    let main_data =
        slice::from_raw_parts(transaction.data.buffer as *const u8, transaction.data_size);
    let offsets = slice::from_raw_parts(
        transaction.data.offsets as *const usize,
        transaction.offsets_size / size_of::<usize>(),
    );
    for offset in offsets {
        let header_bytes = &main_data[*offset..offset + size_of::<BinderObjectHeader>()];
        let header =
            unsafe { ptr::read_unaligned(header_bytes.as_ptr() as *const BinderObjectHeader) };
        match header.type_ {
            BinderType::BINDER => {
                let binder_obj_bytes = &main_data[*offset..offset + size_of::<FlatBinderObject>()];
                let flat_obj = binder_obj_bytes.as_ptr() as *const FlatBinderObject;
                let flat_obj = unsafe { flat_obj.as_ref().unwrap() };
                let id = BinderObjectId::from_raw(flat_obj.data.binder, flat_obj.cookie);
                f(id);
            }
            _ => continue,
        }
    }
}

/// Speculatively marks every locally-owned object embedded in an about-to-be-sent
/// transaction as awaiting a remote acquire. Must be paired with
/// [`unmark_objects_as_pending_remote`] if the transaction turns out not to actually
/// deliver (write failure, dead reply, peer death, ...), or those objects can never be
/// freed again — see [`ObjectRefState::clear_pending_remote`].
unsafe fn mark_objects_as_pending_remote(dev: &BinderDevice, transaction: &BinderTransactionData) {
    unsafe {
        for_each_embedded_binder_object(transaction, |id| {
            tracing::trace!(?id, "found binder object id in transaction");
            if let Some(refcount) = dev.object_refcounts.get(&id) {
                refcount.mark_pending_remote();
            } else {
                tracing::warn!(
                    ?id,
                    "binder object found in transaction but it has no refcounts"
                );
            }
        });
    }
}

/// Rolls back [`mark_objects_as_pending_remote`] for a transaction that is now known to
/// have failed, so a BR_ACQUIRE that will never arrive doesn't leave those objects
/// permanently un-collectible.
unsafe fn unmark_objects_as_pending_remote(
    dev: &BinderDevice,
    transaction: &BinderTransactionData,
) {
    unsafe {
        for_each_embedded_binder_object(transaction, |id| {
            if let Some(refcount) = dev.object_refcounts.get(&id) {
                refcount.clear_pending_remote();
            }
        });
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

#[instrument(level = "trace", skip(runtime, device))]
fn looper(
    runtime: &tokio::runtime::Handle,
    device: Weak<BinderDevice>,
    dev_fd: Arc<OwnedFd>,
    spawned: bool,
) {
    let _guard = runtime.enter();
    let cmd = if spawned {
        BinderCommand::REGISTER_LOOPER
    } else {
        BinderCommand::ENTER_LOOPER
    };
    let mut init_data = Vec::new();
    init_data.extend_from_slice(&cmd.as_u32().to_ne_bytes());
    let mut init_data = Some(init_data.as_slice());
    loop {
        match unsafe { binder_write_read(&dev_fd, init_data.take(), &device, runtime) } {
            Some(Ok(_)) => {
                error!("looper unexpectedly received a reply, ignoring");
            }
            Some(Err(WriteReadError::NoDevice)) => {
                break;
            }
            Some(Err(WriteReadError::DeadReply)) => {}
            Some(Err(WriteReadError::ObjectNotFound)) => {}
            Some(Err(WriteReadError::FailedReply)) => {
                error!("looper {}", WriteReadError::FailedReply);
            }
            Some(Err(WriteReadError::FrozenReply)) => {
                debug!("looper: transaction target was frozen");
            }
            Some(Err(WriteReadError::AsyncBufferFull)) => {
                debug!("looper: async buffer full");
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
#[instrument(level = "trace", skip(write_data, device, runtime))]
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
    let _guard = trace_span!("ioctl wait").entered();
    let res = io::retry_on_intr(|| {
        let _trace = trace_span!("ioctl try").entered();
        unsafe { rustix::ioctl::ioctl(dev_fd, &mut binder_wr) }
    });
    drop(_guard);
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
                // Create payload_reader before the handler lookup so its Drop always sends
                // FREE_BUFFER to the kernel, even if we bail out early. Without this, a
                // missing handler silently leaks the transaction buffer until it fills up.
                let payload_reader = unsafe {
                    PayloadReader::from_kernel_raw(
                        device.clone(),
                        transaction.data.buffer as *const u8,
                        transaction.data_size,
                        transaction.data.offsets as *const usize,
                        transaction.offsets_size / size_of::<usize>(),
                    )
                };
                let handler = {
                    let Some(entry) = device.objects.get(&target) else {
                        warn!("unable to find handler for: {target:?}");
                        // Drop payload_reader first to send FREE_BUFFER before any reply.
                        drop(payload_reader);
                        if !transaction.flags.contains(TransactionFlags::ONE_WAY) {
                            // Send an empty reply so the remote caller isn't blocked forever.
                            let empty_reply = BinderTransactionData {
                                target: sys::TransactionTarget { binder: 0 },
                                cookie: 0,
                                code: transaction.code,
                                flags: transaction.flags,
                                sender_pid: rustix::process::getpid().as_raw_pid(),
                                sender_euid: rustix::process::getuid().as_raw(),
                                data_size: 0,
                                offsets_size: 0,
                                data: crate::sys::BinderTransactionDataPtrs {
                                    buffer: 0,
                                    offsets: 0,
                                },
                            };
                            let mut bytes = Vec::new();
                            bytes.extend_from_slice(&BinderCommand::REPLY.as_u32().to_ne_bytes());
                            bytes.extend_from_slice(unsafe {
                                slice::from_raw_parts(
                                    &raw const empty_reply as _,
                                    size_of_val(&empty_reply),
                                )
                            });
                            write_binder_command(dev_fd, &bytes).unwrap();
                        }
                        return Some(Err(WriteReadError::ObjectNotFound));
                    };
                    entry.clone()
                };
                if transaction.flags.contains(TransactionFlags::ONE_WAY) {
                    let _guard = trace_span!("Handle oneway transaction").entered();
                    runtime.block_on(handler.handle_one_way(Transaction {
                        code: transaction.code,
                        payload: payload_reader,
                        sender_pid: transaction.sender_pid,
                        sender_euid: transaction.sender_euid,
                    }));
                } else {
                    let _guard = trace_span!("Handle transaction").entered();
                    let reply_data = runtime.block_on(handler.handle(Transaction {
                        code: transaction.code,
                        payload: payload_reader,
                        sender_pid: transaction.sender_pid,
                        sender_euid: transaction.sender_euid,
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
                    // Protect any freshly-embedded local objects from a
                    // premature `strong_refs_hit_zero` firing (via
                    // `drop(reply_data)` below) before the remote side has
                    // actually sent BR_ACQUIRE for them — mirrors the same
                    // marking done for outgoing calls in
                    // `remote_transact_blocking`/`remote_transact_one_way`.
                    // Cleared later by `increase_remote()` once the real
                    // acquire arrives.
                    unsafe { mark_objects_as_pending_remote(&device, &reply) };
                    let mut bytes = Vec::new();
                    bytes.extend_from_slice(&BinderCommand::REPLY.as_u32().to_ne_bytes());
                    bytes.extend_from_slice(slice::from_raw_parts(
                        &raw const reply as _,
                        size_of_val(&reply),
                    ));
                    // Sending BC_REPLY with a freshly-embedded local object can make
                    // the kernel queue our own BR_ACQUIRE for it onto *this exact*
                    // kernel thread's work list as a side effect of processing the
                    // write (binder_inc_node_nilocked in the kernel enqueues it to
                    // the calling thread's `thread->todo`, which always has
                    // priority over the process-wide queue on the next read). If we
                    // write via a read-less ioctl (like plain `write_binder_command`,
                    // which sets read_size=0), that queued BR_ACQUIRE just sits
                    // there competing with whatever new incoming transaction this
                    // thread picks up next — under enough concurrent load, this
                    // thread can keep being handed fresh transactions instead of
                    // ever coming back around to drain its own queue, leaving the
                    // object's `new_in_remote` guard (and thus
                    // `strong_refs_hit_zero`) stuck forever. Doing the write and the
                    // very next read in the *same* ioctl call (like every other
                    // looper turn does) guarantees the kernel processes this
                    // thread's own queued BR_ACQUIRE immediately, in this call.
                    match unsafe {
                        binder_write_read(dev_fd, Some(&bytes), &Arc::downgrade(&device), runtime)
                    } {
                        Some(Ok(_)) => {
                            error!(
                                "looper unexpectedly received a reply while sending BC_REPLY, ignoring"
                            );
                        }
                        Some(Err(e)) => {
                            // The reply never made it out, so no BR_ACQUIRE will
                            // ever arrive for the objects we just marked —
                            // roll the marking back so they're still
                            // collectible instead of stuck pending forever.
                            error!("failed to write BC_REPLY: {e}");
                            unsafe { unmark_objects_as_pending_remote(&device, &reply) };
                        }
                        None => {}
                    }
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
                let target = unsafe {
                    read_from_slice::<BinderPtrCookie>(&read_slice[header..], &mut consumed)
                };
                let id = BinderObjectId::from_raw(target.ptr, target.cookie);
                if let Some(refstate) = device.object_refcounts.get(&id) {
                    refstate.increase_remote();
                }
                _ = write_binder_struct_command(dev_fd, BinderCommand::ACQUIRE_DONE, &target)
                    .inspect_err(|err| error!("failed to send ACQUIRE_DONE: {err}"));
            }
            BinderReturn::RELEASE => {
                let target = unsafe {
                    read_from_slice::<BinderPtrCookie>(&read_slice[header..], &mut consumed)
                };
                let id = BinderObjectId::from_raw(target.ptr, target.cookie);
                if let Some(refstate) = device.object_refcounts.get(&id) {
                    refstate.decrease_remote();
                }
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
                // trace!("noop?");
            }
            BinderReturn::SPAWN_LOOPER => {
                let device = Arc::downgrade(&device);
                let dev_fd = dev_fd.clone();
                let runtime = runtime.clone();
                let _ = std::thread::Builder::new()
                    .name("Requested binder looper".into())
                    .spawn(move || looper(&runtime, device, dev_fd, true));
            }
            BinderReturn::FINISHED => {
                debug!("finished?");
            }
            BinderReturn::DEAD_BINDER => {
                let v = unsafe {
                    read_from_slice::<BinderUintptrT>(&read_slice[header..], &mut consumed)
                };
                if let Some((_, death)) = device.death_notifications.remove(&v) {
                    death.cancel();
                } else {
                    warn!(
                        "got DeadBinder without having internal death_notification registered for it"
                    );
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
                let extended = unsafe { device.get_last_error() };
                if extended.param == -rustix::io::Errno::NOSPC.raw_os_error() {
                    return Some(Err(WriteReadError::AsyncBufferFull));
                }
                warn!("failed reply: {:?}", extended);
                return Some(Err(WriteReadError::FailedReply));
            }
            BinderReturn::FROZEN_REPLY => {
                return Some(Err(WriteReadError::FrozenReply));
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
    #[error("Frozen Reply")]
    FrozenReply,
    #[error("Async buffer full (ENOSPC)")]
    AsyncBufferFull,
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

pub trait TransactionHandler: Any + Debug + Send + Sync + 'static {
    fn type_name(&self) -> &'static str {
        type_name_of_val(self)
    }
    fn handle(
        self: Arc<Self>,
        transaction: Transaction,
    ) -> impl Future<Output = PayloadBuilder<'static>> + Send;
    /// you need to drop the transaction payload for the transaction to be counted as "handled",
    /// please whatever you do, copy the data out as soon as absolutly possible and drop the
    /// payload, else this will cause deadlocks and freeze, please, trust me
    fn handle_one_way(self: Arc<Self>, transaction: Transaction)
    -> impl Future<Output = ()> + Send;
}

pub(crate) trait ErasedTransactionHandler: Any + Debug + Send + Sync + 'static {
    fn type_name(&self) -> &'static str;
    fn handle(
        self: Arc<Self>,
        transaction: Transaction,
    ) -> Pin<Box<dyn Future<Output = PayloadBuilder<'static>> + Send>>;
    fn handle_one_way(
        self: Arc<Self>,
        transaction: Transaction,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

impl<T: TransactionHandler> ErasedTransactionHandler for T {
    fn type_name(&self) -> &'static str {
        self.type_name()
    }
    fn handle(
        self: Arc<Self>,
        transaction: Transaction,
    ) -> Pin<Box<dyn Future<Output = PayloadBuilder<'static>> + Send>> {
        Box::pin(TransactionHandler::handle(self, transaction))
    }
    fn handle_one_way(
        self: Arc<Self>,
        transaction: Transaction,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(TransactionHandler::handle_one_way(self, transaction))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::PayloadBuilder;
    use crate::test_pool::PoolNode;
    use std::sync::Arc;

    #[derive(Debug)]
    struct NoopService;

    impl TransactionHandler for NoopService {
        async fn handle(self: Arc<Self>, _tx: Transaction) -> PayloadBuilder<'static> {
            PayloadBuilder::new()
        }
        async fn handle_one_way(self: Arc<Self>, _tx: Transaction) {}
    }

    /// These tests never call `set_context_manager`, so nothing else on the
    /// node could conflict with them — the node lock only needs to be held
    /// long enough to open the device, and is released as soon as `node`
    /// goes out of scope.
    fn open_device() -> Arc<BinderDevice> {
        let node = PoolNode::acquire();
        BinderDevice::new(&node.path).expect("open pool node")
    }

    /// A plain BinderObject cleans up both maps on drop.
    #[tokio::test]
    async fn binder_object_drops_cleanly() {
        let dev = open_device();
        let obj: crate::binder_object::BinderObject<NoopService> =
            dev.register_object(Arc::new(NoopService));
        let id = obj.id;

        assert!(dev.objects.contains_key(&id));
        assert!(dev.object_refcounts.contains_key(&id));

        drop(obj);

        assert!(
            !dev.objects.contains_key(&id),
            "object still in objects after drop"
        );
        assert!(
            !dev.object_refcounts.contains_key(&id),
            "refcounts still present after drop"
        );
    }

    /// to_service() keeps the guard alive until the last BinderObjectRef is dropped,
    /// then the cleanup task removes it from retained_services and objects.
    #[tokio::test]
    async fn service_drops_when_last_ref_gone() {
        let dev = open_device();
        let handler = Arc::new(NoopService);
        let weak_handler = Arc::downgrade(&handler);

        let obj: crate::binder_object::BinderObject<NoopService> = dev.register_object(handler);
        let id = obj.id;

        let service_ref = obj.to_service();

        assert!(
            dev.retained_services.contains_key(&id),
            "service not retained after to_service"
        );
        assert!(dev.objects.contains_key(&id));
        assert!(
            weak_handler.upgrade().is_some(),
            "handler dropped too early"
        );

        drop(service_ref);

        // Yield a few times so the spawned cleanup task runs.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        assert!(
            !dev.retained_services.contains_key(&id),
            "retained_services not cleaned up after last ref dropped"
        );
        assert!(!dev.objects.contains_key(&id), "objects not cleaned up");
        assert!(
            weak_handler.upgrade().is_none(),
            "handler Arc not fully dropped"
        );
    }

    /// strong_refs_hit_zero() resolves as soon as the last local ref is dropped.
    #[tokio::test]
    async fn strong_refs_hit_zero_fires_on_last_drop() {
        let dev = open_device();
        let obj: crate::binder_object::BinderObject<NoopService> =
            dev.register_object(Arc::new(NoopService));

        let service_ref = obj.to_service();
        let hit_zero = service_ref.strong_refs_hit_zero();

        // Unlike the old `Notify`-based implementation, dropping inline
        // (before `hit_zero` is ever polled) is safe: `strong_refs_hit_zero`
        // subscribes to the underlying `watch` eagerly, and `wait_for`
        // checks the *current* value first rather than only catching an
        // edge it happened to be listening for.
        drop(service_ref);

        tokio::time::timeout(std::time::Duration::from_millis(500), hit_zero)
            .await
            .expect("strong_refs_hit_zero did not fire within 500 ms");
    }

    /// Cloning a BinderObjectRef delays cleanup until every clone is dropped.
    #[tokio::test]
    async fn service_survives_until_all_clones_dropped() {
        let dev = open_device();
        let handler = Arc::new(NoopService);
        let weak_handler = Arc::downgrade(&handler);

        let obj: crate::binder_object::BinderObject<NoopService> = dev.register_object(handler);
        let id = obj.id;

        let ref_a = obj.to_service();
        let ref_b = ref_a.clone();

        // Drop first clone — service must still be alive.
        drop(ref_a);
        tokio::task::yield_now().await;

        assert!(
            dev.retained_services.contains_key(&id),
            "service cleaned up while a clone still exists"
        );
        assert!(
            weak_handler.upgrade().is_some(),
            "handler dropped while ref_b is alive"
        );

        // Drop the last clone — service may now be cleaned up.
        drop(ref_b);
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        assert!(
            !dev.retained_services.contains_key(&id),
            "retained_services not cleaned up after all clones dropped"
        );
        assert!(
            weak_handler.upgrade().is_none(),
            "handler Arc not fully dropped"
        );
    }

    /// White-box regression test for a "dropped too early" bug: the same
    /// object embedded in two concurrent outgoing sends, one of which fails
    /// and rolls back. The failing send's rollback must only cancel its own
    /// guard, not the other (still legitimately in-flight) send's — driven
    /// directly against `ObjectRefState` since reliably forcing one of two
    /// concurrent real kernel sends to fail while the other succeeds isn't
    /// practical to arrange from outside the crate.
    #[test]
    fn concurrent_pending_sends_do_not_cross_cancel() {
        let id = BinderObjectId { id: 1, cookie: 0 };
        let state = ObjectRefState::new(id);
        let mut rx = state.subscribe();

        // Two concurrent sends of the same never-before-remote object, both
        // guarding against the object's local count (transiently 0 between
        // the two marks below) hitting a premature zero.
        state.mark_pending_remote(); // send A
        state.mark_pending_remote(); // send B
        assert_eq!(*rx.borrow_and_update(), 2, "both sends should be guarded");

        // Send B fails and rolls back its own guard — send A is still in
        // flight and must remain protected.
        state.clear_pending_remote();
        assert_eq!(
            *rx.borrow_and_update(),
            1,
            "rolling back send B must not cancel send A's still-legitimate guard"
        );

        // Send A's real BR_ACQUIRE lands.
        state.increase_remote();
        assert_eq!(
            *rx.borrow_and_update(),
            1,
            "send A's remote ref is now established"
        );

        state.decrease_remote();
        assert_eq!(
            *rx.borrow_and_update(),
            0,
            "genuinely nothing outstanding now"
        );
    }

    /// Several concurrent sends can all race `mark_pending_remote` while
    /// `remote` is still 0, but the kernel only ever queues *one* BR_ACQUIRE
    /// for the actual 0->1 transition — the "losing" racers never get an
    /// individual ACQUIRE of their own to consume their credit.
    /// `increase_remote` must resolve *all* of them at once (a full reset),
    /// not just one, or the losers' credits get stuck forever and the
    /// object never reports a real zero later.
    #[test]
    fn single_acquire_resolves_all_racing_pending_sends() {
        let id = BinderObjectId { id: 2, cookie: 0 };
        let state = ObjectRefState::new(id);
        let mut rx = state.subscribe();

        // Three concurrent first-time sends of the same object, all racing.
        state.mark_pending_remote();
        state.mark_pending_remote();
        state.mark_pending_remote();
        assert_eq!(*rx.borrow_and_update(), 3, "all three should be guarded");

        // Only one of them actually wins the race to be the real 0->1
        // transition in the kernel; only one BR_ACQUIRE ever arrives.
        state.increase_remote();
        assert_eq!(
            *rx.borrow_and_update(),
            1,
            "one ACQUIRE must resolve every racing send's guard at once, not just one"
        );

        // The object genuinely goes away later — must still reach zero,
        // not get stuck on the other two racers' never-consumed credits.
        state.decrease_remote();
        assert_eq!(
            *rx.borrow_and_update(),
            0,
            "must reach true zero, not stay stuck"
        );
    }

    /// Isolates whether the watch/mutex plumbing itself (as opposed to
    /// anything binder/kernel-specific) can drop an update: hammer
    /// `ObjectRefState` from real OS threads (matching how looper threads
    /// call it) while a tokio task awaits `wait_for` on a subscribed
    /// receiver, many times over, looking for any hang.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn watch_survives_concurrent_os_thread_hammering() {
        for round in 0..2000 {
            let id = BinderObjectId {
                id: round,
                cookie: 0,
            };
            let state = Arc::new(ObjectRefState::new(id));
            let mut rx = state.subscribe();

            state.increase_local();
            let hit_zero = async {
                rx.wait_for(|count| *count == 0).await.unwrap();
            };

            let s1 = state.clone();
            let s2 = state.clone();
            let t1 = std::thread::spawn(move || {
                s1.increase_local();
                s1.decrease_local();
            });
            let t2 = std::thread::spawn(move || s2.decrease_local());

            let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), hit_zero).await;
            t1.join().unwrap();
            t2.join().unwrap();
            if outcome.is_err() {
                let counts = *state.counts.lock().unwrap();
                panic!(
                    "round {round}: watch never observed zero; current value = {}, counts = {counts:?}",
                    *rx.borrow()
                );
            }
        }
    }
}
