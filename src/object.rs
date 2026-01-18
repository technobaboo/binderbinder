//! Binder object representing a registered service.
//!
//! When a BinderObject is dropped, the associated handler is automatically
//! unregistered from the device (RAII pattern).

use crate::sys::BinderUintptrT;
use std::sync::Weak;

use super::device::BinderDevice;

/// A binder object that represents a registered service handler.
///
/// When dropped, the handler is automatically unregistered from the device.
pub struct BinderObject {
    device: Weak<BinderDevice>,
    pub cookie: BinderUintptrT,
}

impl BinderObject {
    /// Create a new binder object.
    pub(crate) fn new(device: &std::sync::Arc<BinderDevice>, cookie: BinderUintptrT) -> Self {
        Self {
            device: std::sync::Arc::downgrade(device),
            cookie,
        }
    }

    /// Get the cookie for this object.
    pub fn cookie(&self) -> BinderUintptrT {
        self.cookie
    }

    /// Check if the underlying device is still alive.
    pub fn is_alive(&self) -> bool {
        self.device.strong_count() > 0
    }
}

impl Drop for BinderObject {
    fn drop(&mut self) {
        if let Some(device) = self.device.upgrade() {
            device.service_handlers.remove(&self.cookie);
        }
    }
}
