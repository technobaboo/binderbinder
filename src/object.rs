//! Binder object representing a registered service.
//!
//! When a BinderObject is dropped, the associated handler is automatically
//! unregistered from the device (RAII pattern).

use crate::{device::TransactionHandler, sys::BinderUintptrT};
use std::sync::{Arc, Weak};

use super::device::BinderDevice;

/// A binder object that represents a registered service handler.
///
/// When dropped, the handler is automatically unregistered from the device.
pub struct BinderObject<T: TransactionHandler> {
    pub(crate) device: Weak<BinderDevice>,
    pub handler: Arc<T>,
    pub cookie: BinderUintptrT,
}
impl<T: TransactionHandler> BinderObject<T> {
    /// Get the cookie for this object.
    pub fn cookie(&self) -> BinderUintptrT {
        self.cookie
    }

    /// Check if the underlying device is still alive.
    pub fn is_alive(&self) -> bool {
        self.device.strong_count() > 0
    }
}
impl<T: TransactionHandler> Drop for BinderObject<T> {
    fn drop(&mut self) {
        if let Some(device) = self.device.upgrade() {
            device.service_handlers.remove(&self.cookie);
        }
    }
}
