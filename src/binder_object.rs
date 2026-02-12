use std::{
    fmt::Debug,
    sync::{atomic::AtomicBool, Arc},
};

use tracing::{info, warn};

use crate::{
    device::DynTransactionHandler,
    sys::{
        BinderCommand, BinderObjectHeader, BinderType, BinderUintptrT, FlatBinderFlags,
        FlatBinderObject, FlatBinderObjectData,
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
}

#[derive(Debug)]
/// The remote side of a [`BinderObject`]
pub struct BinderRef {
    device: Arc<BinderDevice>,
    id: u32,
    dead: Arc<AtomicBool>,
}

#[derive(Debug)]
/// Weak version of [`BinderRef`]
pub struct WeakBinderRef {
    device: Arc<BinderDevice>,
    id: u32,
    dead: Arc<AtomicBool>,
}

impl BinderRef {
    pub fn get_context_manager_handle(device: &Arc<BinderDevice>) -> Arc<Self> {
        Self {
            device: device.clone(),
            id: 0,
            dead: AtomicBool::new(false).into(),
        }
        .into()
    }
    pub fn downgrade(&self) -> Option<Arc<WeakBinderRef>> {
        let handle = self
            .device
            .weak_port_handles
            .get(&self.id)
            .and_then(|v| v.upgrade());
        if let Some(handle) = handle {
            Some(handle)
        } else {
            warn!("Failed to find exising weak handle, proper downgrade unimplemented, returning None");
            None
        }
    }
    pub(crate) fn handle(&self) -> u32 {
        self.id
    }
    /// this should only be called when receiving a new handle
    pub(crate) fn get_and_dedup_from_raw(device: &Arc<BinderDevice>, handle: u32) -> Arc<Self> {
        if let Some(port) = device.port_handles.get(&handle).and_then(|v| v.upgrade()) {
            // TODO: dedup kernel strong ref
            warn!("dedupped BinderPortHandle, proper kernel ref deduping currently unimplemented, leaking strong refs");
            return port;
        }
        unsafe {
            info!("increasing ref counts?");
            device.write_binder_struct_command(BinderCommand::ACQUIRE, &handle);
            device.write_binder_struct_command(BinderCommand::INCREFS, &handle);
        }
        let port = Arc::new(Self {
            device: device.clone(),
            id: handle,
            dead: Arc::new(AtomicBool::new(false)),
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
            data: FlatBinderObjectData { handle: self.id },
            // ignored for non local ports
            cookie: 0,
        }
    }
}

impl WeakBinderRef {
    pub fn upgrade(&self) -> Option<Arc<BinderRef>> {
        let handle = self
            .device
            .port_handles
            .get(&self.id)
            .and_then(|v| v.upgrade());
        if let Some(handle) = handle {
            Some(handle)
        } else {
            warn!("Failed to find exising strong handle, proper upgrade unimplemented, returning None");
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
            // TODO: dedup kernel strong ref
            warn!("dedupped BinderPortHandle, proper kernel ref deduping currently unimplemented, leaking strong refs");
            return port;
        }
        unsafe {
            info!("increasing ref counts?");
            device.write_binder_struct_command(BinderCommand::INCREFS, &handle);
            device.write_binder_struct_command(BinderCommand::ACQUIRE, &handle);
        }
        let port = Arc::new(Self {
            device: device.clone(),
            id: handle,
            dead: Arc::new(AtomicBool::new(false)),
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

/// Only returned if a remote process sends a [`WeakBinderRef`] to the process owning the [`BinderObject`]
#[derive(Debug)]
pub struct WeakBinderObject {
    // TODO: is this needed?
    device: Arc<BinderDevice>,
    id: BinderObjectId,
}

impl WeakBinderObject {
    pub fn id(&self) -> &BinderObjectId {
        &self.id
    }
    pub(crate) fn from_id(device: Arc<BinderDevice>, id: BinderObjectId) -> Self {
        Self { device, id }
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
