use std::{
    fmt::Debug,
    future::Future,
    ops::Deref,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use tokio::sync::Notify;
use tracing::{info, warn};

use crate::{
    device::DynTransactionHandler,
    sys::{
        BinderCommand, BinderHandleCookie, BinderObjectHeader, BinderType, BinderUintptrT,
        FlatBinderFlags, FlatBinderObject, FlatBinderObjectData,
    },
    BinderDevice,
};

/// Used to send or receive transactions, roughly maps onto the uapi `flat_binder_object`.
#[derive(Debug)]
pub enum BinderObjectOrRef {
    Object(Arc<BinderObject>),
    WeakObject(WeakBinderObject),
    Ref(Arc<BinderRef>),
    WeakRef(Arc<WeakBinderRef>),
}
impl BinderObjectOrRef {
    pub(crate) fn get_flat_binder_object(&self) -> FlatBinderObject {
        match self {
            BinderObjectOrRef::Object(p) => p.get_flat_binder_object(),
            BinderObjectOrRef::Ref(p) => p.get_flat_binder_object(),
            BinderObjectOrRef::WeakRef(p) => p.get_flat_binder_object(),
            BinderObjectOrRef::WeakObject(p) => p.get_flat_binder_object(),
        }
    }
    pub fn alive(&self) -> bool {
        match self {
            BinderObjectOrRef::Object(_) => true,
            BinderObjectOrRef::WeakObject(_) => true,
            BinderObjectOrRef::Ref(p) => !p.dead.load(Ordering::Relaxed),
            BinderObjectOrRef::WeakRef(p) => !p.dead.load(Ordering::Relaxed),
        }
    }
}

/// The remote side of a [`BinderObject`]
#[derive(Debug)]
pub struct BinderRef {
    device: Arc<BinderDevice>,
    weak: Arc<WeakBinderRef>,
}

impl Deref for BinderRef {
    type Target = Arc<WeakBinderRef>;

    fn deref(&self) -> &Self::Target {
        &self.weak
    }
}

/// Weak version of [`BinderRef`]
#[derive(Debug)]
pub struct WeakBinderRef {
    device: Arc<BinderDevice>,
    id: u32,
    dead: Arc<AtomicBool>,
    death_notify: Arc<Notify>,
}

impl BinderRef {
    pub fn downgrade(&self) -> Arc<WeakBinderRef> {
        self.weak.clone()
    }
    pub(crate) fn handle(&self) -> u32 {
        self.weak.handle()
    }
    /// this should only be called when receiving a new handle
    pub(crate) fn get_and_dedup_from_raw(device: &Arc<BinderDevice>, handle: u32) -> Arc<Self> {
        if let Some(port) = device.port_handles.get(&handle).and_then(|v| v.upgrade()) {
            return port;
        }
        let weak = WeakBinderRef::get_and_dedup_from_raw(device, handle);
        unsafe {
            device.write_binder_struct_command(BinderCommand::ACQUIRE, &handle);
        }
        let port = Arc::new(Self {
            device: device.clone(),
            weak,
        });
        device.port_handles.insert(handle, Arc::downgrade(&port));
        port
    }
    pub(crate) fn get_flat_binder_object(&self) -> FlatBinderObject {
        FlatBinderObject {
            hdr: BinderObjectHeader {
                type_: BinderType::HANDLE,
            },
            // TODO: handle actual flags
            flags: FlatBinderFlags::ACCEPTS_FDS,
            data: FlatBinderObjectData {
                handle: self.handle(),
            },
            // ignored for non local ports
            cookie: 0,
        }
    }
}
impl TransactionTarget for BinderRef {}
impl TransactionTargetImpl for BinderRef {
    fn get_transaction_target_handle(&self) -> TransactionTargetHandle {
        TransactionTargetHandle::Remote(self.handle())
    }
}
impl Drop for BinderRef {
    fn drop(&mut self) {
        unsafe {
            self.device
                .write_binder_struct_command(BinderCommand::RELEASE, &self.handle());
        }
    }
}

impl WeakBinderRef {
    /// future returns when the remote object died
    pub fn death_notification(&self) -> impl Future<Output = ()> + 'static {
        let notify = self.death_notify.clone();
        let dead = self.dead.clone();
        async move {
            if !dead.load(Ordering::Relaxed) {
                notify.notified().await
            }
        }
    }
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }
    pub fn upgrade(&self) -> Option<Arc<BinderRef>> {
        let handle = self
            .device
            .port_handles
            .get(&self.id)
            .and_then(|v| v.upgrade());
        if let Some(handle) = handle {
            Some(handle)
        } else {
            warn!("Failed to find exising strong handle");
            None
        }
    }
    pub(crate) fn handle(&self) -> u32 {
        self.id
    }
    /// this should only be called when receiving a new handle
    pub(crate) fn get_and_dedup_from_raw(device: &Arc<BinderDevice>, handle: u32) -> Arc<Self> {
        if let Some(port) = device
            .weak_port_handles
            .get(&handle)
            .and_then(|v| v.upgrade())
        {
            return port;
        }
        let death_notif_cookie = device.death_counter.fetch_add(1, Ordering::Relaxed);
        let death_notify = Arc::new(Notify::new());
        device
            .death_notifications
            .insert(death_notif_cookie, death_notify.clone());
        let dead = Arc::new(AtomicBool::new(false));
        tokio::spawn({
            let dead = dead.clone();
            let notify = death_notify.clone();
            async move {
                notify.notified().await;
                dead.store(true, Ordering::Relaxed);
            }
        });
        unsafe {
            device.write_binder_struct_command(BinderCommand::INCREFS, &handle);
            device.write_binder_struct_command(
                BinderCommand::REQUEST_DEATH_NOTIFICATION,
                &BinderHandleCookie {
                    handle,
                    cookie: death_notif_cookie,
                },
            );
        }
        let port = Arc::new(Self {
            device: device.clone(),
            id: handle,
            dead,
            death_notify,
        });
        device
            .weak_port_handles
            .insert(handle, Arc::downgrade(&port));
        port
    }
    pub(crate) fn get_flat_binder_object(&self) -> FlatBinderObject {
        FlatBinderObject {
            hdr: BinderObjectHeader {
                type_: BinderType::WEAK_HANDLE,
            },
            // TODO: handle actual flags
            flags: FlatBinderFlags::ACCEPTS_FDS,
            data: FlatBinderObjectData { handle: self.id },
            // ignored for non local ports
            cookie: 0,
        }
    }
}
impl TransactionTarget for WeakBinderRef {}
impl TransactionTargetImpl for WeakBinderRef {
    fn get_transaction_target_handle(&self) -> TransactionTargetHandle {
        TransactionTargetHandle::Remote(self.handle())
    }
}
impl Drop for WeakBinderRef {
    fn drop(&mut self) {
        unsafe {
            self.device
                .write_binder_struct_command(BinderCommand::DECREFS, &self.handle());
        }
    }
}

/// The id of a [`BinderObject`]
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct BinderObjectId {
    pub(crate) id: BinderUintptrT,
    pub(crate) cookie: BinderUintptrT,
}

/// The owned/local side of a [`BinderRef`]
pub struct BinderObject {
    device: Arc<BinderDevice>,
    id: BinderObjectId,
    handler: Box<dyn DynTransactionHandler>,
}

impl Debug for BinderObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BinderObject")
            .field("device", &self.device)
            .field("id", &self.id)
            .finish()
    }
}

impl BinderObject {
    pub fn id(&self) -> &BinderObjectId {
        &self.id
    }
    pub(crate) fn handler(&self) -> &dyn DynTransactionHandler {
        self.handler.as_ref()
    }
    pub(crate) fn new(
        id: usize,
        handler: Box<dyn DynTransactionHandler>,
        device: Arc<BinderDevice>,
    ) -> Arc<Self> {
        Self {
            device,
            id: BinderObjectId { id, cookie: 0 },
            handler,
        }
        .into()
    }
    pub(crate) fn get_flat_binder_object(&self) -> FlatBinderObject {
        FlatBinderObject {
            hdr: BinderObjectHeader {
                type_: BinderType::BINDER,
            },
            // TODO: handle actual flags
            flags: FlatBinderFlags::ACCEPTS_FDS,
            data: FlatBinderObjectData { binder: self.id.id },
            cookie: self.id.cookie,
        }
    }
}
impl TransactionTarget for BinderObject {}
impl TransactionTargetImpl for BinderObject {
    fn get_transaction_target_handle(&self) -> TransactionTargetHandle {
        TransactionTargetHandle::Local(*self.id())
    }
}

/// Only returned if a remote process sends a [`WeakBinderRef`] to the process owning the [`BinderObject`]
#[derive(Debug)]
pub struct WeakBinderObject {
    id: BinderObjectId,
}

impl WeakBinderObject {
    pub fn id(&self) -> &BinderObjectId {
        &self.id
    }
    pub(crate) fn from_id(id: BinderObjectId) -> Self {
        Self { id }
    }
    pub(crate) fn get_flat_binder_object(&self) -> FlatBinderObject {
        FlatBinderObject {
            hdr: BinderObjectHeader {
                type_: BinderType::WEAK_BINDER,
            },
            // TODO: handle actual flags
            flags: FlatBinderFlags::ACCEPTS_FDS,
            data: FlatBinderObjectData { binder: self.id.id },
            cookie: self.id.cookie,
        }
    }
}
impl TransactionTarget for WeakBinderObject {}
impl TransactionTargetImpl for WeakBinderObject {
    fn get_transaction_target_handle(&self) -> TransactionTargetHandle {
        TransactionTargetHandle::Local(*self.id())
    }
}

impl BinderObjectId {
    pub(crate) fn from_raw(binder: BinderUintptrT, cookie: BinderUintptrT) -> BinderObjectId {
        Self { id: binder, cookie }
    }
}

impl Drop for BinderObject {
    fn drop(&mut self) {
        self.device.remove_binder_port(&self.id);
    }
}
#[allow(private_bounds)]
pub trait TransactionTarget: TransactionTargetImpl {}
pub(crate) enum TransactionTargetHandle {
    Local(BinderObjectId),
    Remote(u32),
}
pub(crate) trait TransactionTargetImpl {
    fn get_transaction_target_handle(&self) -> TransactionTargetHandle;
}
pub struct ContextManagerBinderRef;
impl TransactionTarget for ContextManagerBinderRef {}
impl TransactionTargetImpl for ContextManagerBinderRef {
    fn get_transaction_target_handle(&self) -> TransactionTargetHandle {
        TransactionTargetHandle::Remote(0)
    }
}
impl TransactionTarget for BinderObjectOrRef {}
impl TransactionTargetImpl for BinderObjectOrRef {
    fn get_transaction_target_handle(&self) -> TransactionTargetHandle {
        match self {
            BinderObjectOrRef::Object(v) => v.get_transaction_target_handle(),
            BinderObjectOrRef::WeakObject(v) => v.get_transaction_target_handle(),
            BinderObjectOrRef::Ref(v) => v.get_transaction_target_handle(),
            BinderObjectOrRef::WeakRef(v) => v.get_transaction_target_handle(),
        }
    }
}
