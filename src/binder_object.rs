use std::{
    fmt::Debug,
    future::Future,
    hash::{Hash, Hasher},
    ops::Deref,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
        Arc, Weak,
    },
};

use tokio::sync::Notify;
use tracing::warn;

use crate::{
    device::{DynBinderObject, Transaction},
    payload::PayloadBuilder,
    sys::{
        BinderCommand, BinderHandleCookie, BinderObjectHeader, BinderType, BinderUintptrT,
        FlatBinderFlags, FlatBinderObject, FlatBinderObjectData,
    },
    BinderDevice, TransactionHandler,
};

/// Used to send or receive transactions, roughly maps onto the uapi `flat_binder_object`.
#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub enum BinderObjectOrRef {
    Object(UntypedBinderObject),
    WeakObject(WeakBinderObject),
    Ref(Arc<BinderRef>),
    WeakRef(Arc<WeakBinderRef>),
}
impl BinderObjectOrRef {
    pub(crate) fn get_flat_binder_object(&self) -> FlatBinderObject {
        match self {
            BinderObjectOrRef::Object(p) => p.0.get_flat_binder_object(),
            BinderObjectOrRef::Ref(p) => p.get_flat_binder_object(),
            BinderObjectOrRef::WeakRef(p) => p.get_flat_binder_object(),
            BinderObjectOrRef::WeakObject(p) => p.get_flat_binder_object(),
        }
    }
    /// returns true if this is an object, or [`WeakBinderRef::alive`] if this is a ref
    pub fn alive(&self) -> bool {
        match self {
            BinderObjectOrRef::Object(_) => true,
            BinderObjectOrRef::WeakObject(_) => true,
            BinderObjectOrRef::Ref(p) => p.alive(),
            BinderObjectOrRef::WeakRef(p) => p.alive(),
        }
    }
    pub fn device(&self) -> &Arc<BinderDevice> {
        match self {
            BinderObjectOrRef::Object(p) => p.device(),
            BinderObjectOrRef::WeakObject(p) => &p.device,
            BinderObjectOrRef::Ref(p) => &p.device,
            BinderObjectOrRef::WeakRef(p) => &p.device,
        }
    }
}

/// The remote side of a [`BinderObject`]
#[derive(Debug)]
pub struct BinderRef {
    device: Arc<BinderDevice>,
    weak: Arc<WeakBinderRef>,
}
impl Hash for BinderRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.device).hash(state);
        self.weak.hash(state);
    }
}
impl PartialEq for BinderRef {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.device, &other.device) && self.id == other.id
    }
}
impl Eq for BinderRef {}

impl Deref for BinderRef {
    type Target = Arc<WeakBinderRef>;

    fn deref(&self) -> &Self::Target {
        &self.weak
    }
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
        if let Some(port) = device.refs.get(&handle).and_then(|v| v.upgrade()) {
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
        device.refs.insert(handle, Arc::downgrade(&port));
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

/// Weak version of [`BinderRef`]
#[derive(Debug)]
pub struct WeakBinderRef {
    device: Arc<BinderDevice>,
    id: u32,
    dead: Arc<AtomicBool>,
    death_notify: Arc<Notify>,
}

impl Hash for WeakBinderRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.device).hash(state);
        self.id.hash(state);
    }
}
impl PartialEq for WeakBinderRef {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.device, &other.device) && self.id == other.id
    }
}
impl Eq for WeakBinderRef {}

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
    pub fn alive(&self) -> bool {
        !self.dead.load(Ordering::Relaxed)
    }
    pub fn upgrade(&self) -> Option<Arc<BinderRef>> {
        let handle = self.device.refs.get(&self.id).and_then(|v| v.upgrade());
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
        if let Some(port) = device.weak_refs.get(&handle).and_then(|v| v.upgrade()) {
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
        device.weak_refs.insert(handle, Arc::downgrade(&port));
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

#[derive(Debug, Clone)]
pub struct UntypedBinderObject(pub(crate) Arc<dyn DynBinderObject>);
impl UntypedBinderObject {
    pub fn downcast<H: TransactionHandler>(self) -> Option<Arc<BinderObject<H>>> {
        Arc::downcast::<BinderObject<H>>(self.0).ok()
    }
    pub fn device(&self) -> &Arc<BinderDevice> {
        self.0.device()
    }
}

impl Hash for UntypedBinderObject {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.obj_hash(state);
    }
}
impl PartialEq for UntypedBinderObject {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(self.device(), other.device()) && self.0.obj_id() == other.0.obj_id()
    }
}

impl Eq for UntypedBinderObject {}

/// The owned/local side of a [`BinderRef`]
#[derive(Debug)]
pub struct BinderObject<H: TransactionHandler> {
    device: Arc<BinderDevice>,
    id: BinderObjectId,
    strong_count_hit_zero: Notify,
    strong_count_not_zero: Notify,
    strong_count: AtomicU32,
    handler: H,
    object_res: H::ObjectResource,
}

#[async_trait::async_trait]
impl<T: TransactionHandler> DynBinderObject for BinderObject<T> {
    async fn handle(&self, transaction: Transaction) -> PayloadBuilder {
        self.handler.handle(transaction, &self.object_res).await
    }

    async fn handle_one_way(&self, transaction: Transaction) {
        self.handler
            .handle_one_way(transaction, &self.object_res)
            .await
    }

    fn get_flat_binder_object(&self) -> FlatBinderObject {
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
    fn device(&self) -> &Arc<BinderDevice> {
        &self.device
    }
    fn strong_increase(&self) {
        let v = self.strong_count.fetch_add(1, Ordering::Relaxed);
        if v == 0 {
            self.strong_count_not_zero.notify_waiters();
        }
    }
    fn strong_decrease(&self) {
        let v = self.strong_count.fetch_sub(1, Ordering::Relaxed) - 1;
        // strong count hit 0
        if v == 0 {
            self.strong_count_hit_zero.notify_waiters();
        }
    }
    fn obj_hash(&self, mut state: &mut dyn Hasher) {
        self.hash(&mut state);
    }
    fn obj_id(&self) -> &BinderObjectId {
        self.id()
    }
}
impl<H: TransactionHandler> Deref for BinderObject<H> {
    type Target = H;

    fn deref(&self) -> &Self::Target {
        &self.handler
    }
}

impl<H: TransactionHandler> BinderObject<H> {
    pub fn id(&self) -> &BinderObjectId {
        &self.id
    }
    /// Binder strong refs decreased to zero
    pub async fn strong_refs_hit_zero(&self) {
        self.strong_count_hit_zero.notified().await
    }
    /// Binder strong refs increased from zero to above zero
    pub async fn strong_refs_not_zero(&self) {
        self.strong_count_not_zero.notified().await
    }
    pub(crate) fn new(id: usize, handler: H, device: Arc<BinderDevice>) -> Arc<Self> {
        Self {
            device,
            id: BinderObjectId { id, cookie: 0 },
            handler,
            strong_count: AtomicU32::new(0),
            strong_count_hit_zero: Notify::new(),
            strong_count_not_zero: Notify::new(),
            object_res: H::ObjectResource::default(),
        }
        .into()
    }
    pub(crate) fn new_cyclic(
        id: usize,
        f: impl FnOnce(&Weak<Self>) -> H,
        device: Arc<BinderDevice>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            device,
            id: BinderObjectId { id, cookie: 0 },
            handler: f(weak),
            strong_count: AtomicU32::new(0),
            strong_count_hit_zero: Notify::new(),
            strong_count_not_zero: Notify::new(),
            object_res: H::ObjectResource::default(),
        })
    }
}
impl<H: TransactionHandler> TransactionTarget for BinderObject<H> {}
impl<H: TransactionHandler> TransactionTargetImpl for BinderObject<H> {
    fn get_transaction_target_handle(&self) -> TransactionTargetHandle {
        TransactionTargetHandle::Local(*self.id())
    }
}

/// Only returned if a remote process sends a [`WeakBinderRef`] to the process owning the [`BinderObject`]
#[derive(Debug, Clone)]
pub struct WeakBinderObject {
    device: Arc<BinderDevice>,
    id: BinderObjectId,
}

impl Hash for WeakBinderObject {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.device).hash(state);
        self.id.hash(state);
    }
}
impl PartialEq for WeakBinderObject {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.device, &other.device) && self.id == other.id
    }
}
impl Eq for WeakBinderObject {}

impl WeakBinderObject {
    pub fn id(&self) -> &BinderObjectId {
        &self.id
    }
    pub(crate) fn from_id(id: BinderObjectId, device: Arc<BinderDevice>) -> Self {
        Self { id, device }
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

impl<T: TransactionHandler> Hash for BinderObject<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.device).hash(state);
        self.id.hash(state);
    }
}
impl<H: TransactionHandler> PartialEq for BinderObject<H> {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.device, &other.device) && self.id == other.id
    }
}
impl<H: TransactionHandler> Eq for BinderObject<H> {}

impl<H: TransactionHandler> Drop for BinderObject<H> {
    fn drop(&mut self) {
        self.device.remove_binder_object(&self.id);
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
#[derive(Debug)]
pub struct ContextManagerBinderRef(pub(crate) AtomicUsize);
impl TransactionTarget for ContextManagerBinderRef {}
impl TransactionTargetImpl for ContextManagerBinderRef {
    fn get_transaction_target_handle(&self) -> TransactionTargetHandle {
        let id = self.0.load(Ordering::Relaxed);
        if id != 0 {
            TransactionTargetHandle::Local(BinderObjectId { id, cookie: 0 })
        } else {
            TransactionTargetHandle::Remote(0)
        }
    }
}
impl TransactionTarget for BinderObjectOrRef {}
impl TransactionTargetImpl for BinderObjectOrRef {
    fn get_transaction_target_handle(&self) -> TransactionTargetHandle {
        match self {
            BinderObjectOrRef::Object(v) => v.0.get_transaction_target_handle(),
            BinderObjectOrRef::WeakObject(v) => v.get_transaction_target_handle(),
            BinderObjectOrRef::Ref(v) => v.get_transaction_target_handle(),
            BinderObjectOrRef::WeakRef(v) => v.get_transaction_target_handle(),
        }
    }
}
pub trait ToBinderObjectOrRef: Send + Sync + 'static {
    fn to_binder_object_or_ref(&self) -> BinderObjectOrRef;
}
impl<H: TransactionHandler> ToBinderObjectOrRef for Arc<BinderObject<H>> {
    fn to_binder_object_or_ref(&self) -> BinderObjectOrRef {
        BinderObjectOrRef::Object(UntypedBinderObject(self.clone()))
    }
}
impl ToBinderObjectOrRef for WeakBinderObject {
    fn to_binder_object_or_ref(&self) -> BinderObjectOrRef {
        BinderObjectOrRef::WeakObject(WeakBinderObject {
            id: self.id,
            device: self.device.clone(),
        })
    }
}
impl ToBinderObjectOrRef for Arc<BinderRef> {
    fn to_binder_object_or_ref(&self) -> BinderObjectOrRef {
        BinderObjectOrRef::Ref(self.clone())
    }
}
impl ToBinderObjectOrRef for Arc<WeakBinderRef> {
    fn to_binder_object_or_ref(&self) -> BinderObjectOrRef {
        BinderObjectOrRef::WeakRef(self.clone())
    }
}
impl ToBinderObjectOrRef for UntypedBinderObject {
    fn to_binder_object_or_ref(&self) -> BinderObjectOrRef {
        BinderObjectOrRef::Object(self.clone())
    }
}
impl ToBinderObjectOrRef for BinderObjectOrRef {
    fn to_binder_object_or_ref(&self) -> BinderObjectOrRef {
        match self {
            BinderObjectOrRef::Object(v) => v.to_binder_object_or_ref(),
            BinderObjectOrRef::WeakObject(v) => v.to_binder_object_or_ref(),
            BinderObjectOrRef::Ref(v) => v.to_binder_object_or_ref(),
            BinderObjectOrRef::WeakRef(v) => v.to_binder_object_or_ref(),
        }
    }
}
