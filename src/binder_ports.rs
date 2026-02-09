use std::sync::{atomic::AtomicBool, Arc};

use tracing::warn;

use crate::{
    device::DynTransactionHandler,
    sys::{
        BinderObjectHeader, BinderType, BinderUintptrT, FlatBinderFlags, FlatBinderObject,
        FlatBinderObjectData,
    },
    BinderDevice,
};

/// Used to send or receive transactions, roughly maps onto the uapi `flat_binder_object`.
pub enum BinderPort {
    Owned(Arc<OwnedBinderPort>),
    Handle(Arc<BinderPortHandle>),
    WeakHandle(Arc<WeakBinderPortHandle>),
    WeakOwned(WeakOwnedBinderPort),
}
impl BinderPort {
    pub(crate) fn get_flat_binder_object(&self) -> FlatBinderObject {
        match self {
            BinderPort::Owned(p) => p.get_flat_binder_object(),
            BinderPort::Handle(p) => p.get_flat_binder_object(),
            BinderPort::WeakHandle(p) => p.get_flat_binder_object(),
            BinderPort::WeakOwned(p) => p.get_flat_binder_object(),
        }
    }
}

/// The owned/local side of a [`BinderPort`]
pub struct BinderPortHandle {
    device: Arc<BinderDevice>,
    id: u32,
    dead: Arc<AtomicBool>,
}
/// The owned/local side of a [`BinderPort`]
pub struct WeakBinderPortHandle {
    device: Arc<BinderDevice>,
    id: u32,
    dead: Arc<AtomicBool>,
}

impl BinderPortHandle {
    pub fn downgrade(&self) -> Option<Arc<WeakBinderPortHandle>> {
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
    /// this should only be called when receiving a new handle
    pub(crate) fn get_and_dedup_from_raw(device: &Arc<BinderDevice>, handle: u32) -> Arc<Self> {
        if let Some(port) = device.port_handles.get(&handle).and_then(|v| v.upgrade()) {
            // TODO: dedup kernel strong ref
            warn!("dedupped BinderPortHandle, proper kernel ref deduping currently unimplemented, leaking strong refs");
            return port;
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

impl WeakBinderPortHandle {
    pub fn upgrade(&self) -> Option<Arc<BinderPortHandle>> {
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
    /// this should only be called when receiving a new handle
    pub(crate) fn get_and_dedup_from_raw(device: &Arc<BinderDevice>, handle: u32) -> Arc<Self> {
        if let Some(port) = device.weak_port_handles.get(&handle).and_then(|v| v.upgrade()) {
            // TODO: dedup kernel strong ref
            warn!("dedupped BinderPortHandle, proper kernel ref deduping currently unimplemented, leaking strong refs");
            return port;
        }
        let port = Arc::new(Self {
            device: device.clone(),
            id: handle,
            dead: Arc::new(AtomicBool::new(false)),
        });
        device.weak_port_handles.insert(handle, Arc::downgrade(&port));
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

/// The id of a owned/local [`BinderPort`]
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct OwnedBinderPortId {
    pub(crate) id: BinderUintptrT,
    pub(crate) cookie: BinderUintptrT,
}

/// The owned/local side of a [`BinderPort`]
pub struct OwnedBinderPort {
    device: Arc<BinderDevice>,
    id: OwnedBinderPortId,
    handler: Box<dyn DynTransactionHandler>,
}

impl OwnedBinderPort {
    pub fn id(&self) -> &OwnedBinderPortId {
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
            id: OwnedBinderPortId { id, cookie: 0 },
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

/// Only returned if a remote process sends a [`WeakBinderPortHandle`] to the process owning the [`BinderPort`]
pub struct WeakOwnedBinderPort {
    // TODO: is this needed?
    device: Arc<BinderDevice>,
    id: OwnedBinderPortId,
}

impl WeakOwnedBinderPort {
    pub fn id(&self) -> &OwnedBinderPortId {
        &self.id
    }
    pub(crate) fn from_id(device: Arc<BinderDevice>, id: OwnedBinderPortId) -> Self {
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

impl OwnedBinderPortId {
    pub(crate) fn from_raw(binder: BinderUintptrT, cookie: BinderUintptrT) -> OwnedBinderPortId {
        Self { id: binder, cookie }
    }
}

impl Drop for OwnedBinderPort {
    fn drop(&mut self) {
        self.device.remove_binder_port(&self.id);
    }
}
