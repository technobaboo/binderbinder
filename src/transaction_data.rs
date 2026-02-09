use core::slice;
use std::{
    marker::PhantomData,
    os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    ptr::{self, NonNull},
    sync::Arc,
};

use thiserror::Error;

use crate::{
    binder_ports::{
        BinderPort, BinderPortHandle, OwnedBinderPortId, WeakBinderPortHandle, WeakOwnedBinderPort,
    },
    sys::{
        BinderBufferFlags, BinderBufferObject, BinderCommand, BinderFdArrayObject, BinderFdObject,
        BinderObjectHeader, BinderType, FlatBinderObject,
    },
    BinderDevice,
};
pub struct PayloadBuilder<'a> {
    data: Vec<u8>,
    obj_offsets: Vec<usize>,
    buffer_fd_lifetime: PhantomData<&'a ()>,
    owned_fds: Vec<OwnedFd>,
}

impl<'a> PayloadBuilder<'a> {
    pub fn new() -> Self {
        Self {
            data: Vec::new(),
            obj_offsets: Vec::new(),
            buffer_fd_lifetime: PhantomData,
            owned_fds: Vec::new(),
        }
    }
    /// data copied
    pub fn push_bytes(&mut self, bytes: &[u8]) {
        self.data.extend_from_slice(bytes);
    }
    pub fn push_port(&mut self, port: &BinderPort) {
        self.align(align_of::<FlatBinderObject>());
        let flat_obj = port.get_flat_binder_object();
        self.obj_offsets.push(self.data.len());
        let slice = unsafe {
            slice::from_raw_parts(
                &raw const flat_obj as *const u8,
                size_of::<FlatBinderObject>(),
            )
        };
        self.data.extend_from_slice(slice);
    }
    pub fn push_fd<'fd: 'a>(&mut self, fd: BorrowedFd<'fd>, cookie: usize) {
        self.align(align_of::<BinderFdObject>());
        let fd_obj = BinderFdObject {
            hdr: BinderObjectHeader {
                type_: BinderType::FD,
            },
            pad_flags: 0,
            pad_binder: 0,
            fd: fd.as_raw_fd(),
            cookie,
        };
        self.obj_offsets.push(self.data.len());
        let slice = unsafe {
            slice::from_raw_parts(&raw const fd_obj as *const u8, size_of::<BinderFdObject>())
        };
        self.data.extend_from_slice(slice);
    }
    pub fn push_owned_fd(&mut self, fd: OwnedFd, cookie: usize) {
        self.align(align_of::<BinderFdObject>());
        let fd_obj = BinderFdObject {
            hdr: BinderObjectHeader {
                type_: BinderType::FD,
            },
            pad_flags: 0,
            pad_binder: 0,
            fd: fd.as_raw_fd(),
            cookie,
        };
        self.obj_offsets.push(self.data.len());
        let slice = unsafe {
            slice::from_raw_parts(&raw const fd_obj as *const u8, size_of::<BinderFdObject>())
        };
        self.data.extend_from_slice(slice);
        // keep the fd alive
        self.owned_fds.push(fd);
    }
    /// returns offset, used for child buffers
    pub fn push_buffer<'buffer: 'a>(&mut self, bytes: &'buffer [u8]) -> usize {
        self.align(align_of::<BinderBufferObject>());
        let fd_obj = BinderBufferObject {
            hdr: BinderObjectHeader {
                type_: BinderType::PTR,
            },
            flags: BinderBufferFlags::empty(),
            buffer: bytes.as_ptr() as _,
            length: bytes.len(),
            parent: 0,
            parent_offset: 0,
        };
        let offset = self.data.len();
        self.obj_offsets.push(offset);
        let slice = unsafe {
            slice::from_raw_parts(
                &raw const fd_obj as *const u8,
                size_of::<BinderBufferObject>(),
            )
        };
        self.data.extend_from_slice(slice);
        offset
    }
    /// returns offset, used for child buffers
    pub unsafe fn push_child_buffer(
        &mut self,
        bytes: &[u8],
        parent: usize,
        parent_ptr_offset: usize,
    ) -> usize {
        self.align(align_of::<BinderBufferObject>());
        let fd_obj = BinderBufferObject {
            hdr: BinderObjectHeader {
                type_: BinderType::PTR,
            },
            flags: BinderBufferFlags::empty(),
            buffer: bytes.as_ptr() as _,
            length: bytes.len(),
            parent,
            parent_offset: parent_ptr_offset,
        };
        let offset = self.data.len();
        self.obj_offsets.push(offset);
        let slice = unsafe {
            slice::from_raw_parts(
                &raw const fd_obj as *const u8,
                size_of::<BinderBufferObject>(),
            )
        };
        self.data.extend_from_slice(slice);
        offset
    }
    pub unsafe fn push_fd_array(
        &mut self,
        num_fds: usize,
        parent: usize,
        parent_ptr_offset: usize,
    ) {
        self.align(align_of::<BinderFdArrayObject>());
        let fd_obj = BinderFdArrayObject {
            hdr: BinderObjectHeader {
                type_: BinderType::PTR,
            },
            _pad: 0,
            num_fds,
            parent,
            parent_offset: parent_ptr_offset,
        };
        self.obj_offsets.push(self.data.len());
        let slice = unsafe {
            slice::from_raw_parts(
                &raw const fd_obj as *const u8,
                size_of::<BinderFdArrayObject>(),
            )
        };
        self.data.extend_from_slice(slice);
    }
    pub fn align(&mut self, align: usize) {
        while self.data.len() % align != 0 {
            self.data.push(0);
        }
    }
}

impl<'a> PayloadBuilder<'a> {
    pub(crate) fn data_buffer_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }
    pub(crate) fn data_buffer_len(&self) -> usize {
        self.data.len()
    }
    pub(crate) fn offset_buffer_ptr(&self) -> *const usize {
        self.obj_offsets.as_ptr()
    }
    pub(crate) fn offset_buffer_len(&self) -> usize {
        self.obj_offsets.len()
    }
}

pub struct PayloadReader {
    device: Arc<BinderDevice>,
    data_ptr: *const u8,
    data_len: usize,

    offsets_ptr: Option<NonNull<usize>>,
    offsets_len: usize,

    next_offset_index: usize,
    next_data_index: usize,
}
impl PayloadReader {
    pub fn read_bytes(&mut self, num_bytes: usize) -> Result<&[u8], PayloadBytesReadError> {
        let offsets = self
            .offsets_ptr
            .as_ref()
            .map(|ptr| unsafe { slice::from_raw_parts(ptr.as_ptr(), self.offsets_len) });
        let data = unsafe { slice::from_raw_parts(self.data_ptr, self.data_len) };
        // this assumes that all offsets are in order
        if !offsets.is_none_or(|v| {
            v.iter()
                .find(|v| **v > self.next_data_index)
                .is_none_or(|v| self.next_data_index + num_bytes < *v)
        }) {
            return Err(PayloadBytesReadError::ReadingIntoObject);
        }
        if let Some(slice) = data.get(self.next_data_index..self.next_data_index + num_bytes) {
            self.next_data_index += num_bytes;
            Ok(slice)
        } else {
            Err(PayloadBytesReadError::OutOfBounds)
        }
    }
    pub fn read_port(&mut self) -> Result<BinderPort, PayloadPortReadError> {
        let offsets = self
            .offsets_ptr
            .as_ref()
            .map(|ptr| unsafe { slice::from_raw_parts(ptr.as_ptr(), self.offsets_len) });
        let data = unsafe { slice::from_raw_parts(self.data_ptr, self.data_len) };

        let offset = *offsets
            .and_then(|v| v.get(self.next_offset_index))
            .ok_or(PayloadPortReadError::Empty)?;
        let header_bytes = &data[offset..offset + size_of::<BinderObjectHeader>()];
        let header =
            unsafe { ptr::read_unaligned(header_bytes.as_ptr() as *const BinderObjectHeader) };
        if !matches!(
            header.type_,
            BinderType::BINDER
                | BinderType::HANDLE
                | BinderType::WEAK_BINDER
                | BinderType::WEAK_HANDLE
        ) {
            return Err(PayloadPortReadError::IncorrectObject);
        }

        let flat_bytes = &data[offset..offset + size_of::<FlatBinderObject>()];
        let flat_obj =
            unsafe { ptr::read_unaligned(flat_bytes.as_ptr() as *const FlatBinderObject) };
        let port = match flat_obj.hdr.type_ {
            BinderType::BINDER => BinderPort::Owned(
                self.device
                    .owned_ports
                    .get(&OwnedBinderPortId::from_raw(
                        unsafe { flat_obj.data.binder },
                        flat_obj.cookie,
                    ))
                    .ok_or(PayloadPortReadError::UnknownOwnedPort)?
                    .clone(),
            ),
            BinderType::WEAK_BINDER => BinderPort::WeakOwned(WeakOwnedBinderPort::from_id(
                self.device.clone(),
                OwnedBinderPortId::from_raw(unsafe { flat_obj.data.binder }, flat_obj.cookie),
            )),
            BinderType::HANDLE => BinderPort::Handle(BinderPortHandle::get_and_dedup_from_raw(
                &self.device,
                unsafe { flat_obj.data.handle },
            )),
            BinderType::WEAK_HANDLE => BinderPort::WeakHandle(
                WeakBinderPortHandle::get_and_dedup_from_raw(&self.device, unsafe {
                    flat_obj.data.handle
                }),
            ),
            _ => unreachable!("if this is ever reached, horrible things have happened"),
        };
        self.next_offset_index += 1;
        self.next_data_index = offset + size_of::<FlatBinderObject>();
        Ok(port)
    }
    pub fn read_fd(&mut self) -> Result<(OwnedFd, usize), PayloadObjectReadError> {
        let offsets = self
            .offsets_ptr
            .as_ref()
            .map(|ptr| unsafe { slice::from_raw_parts(ptr.as_ptr(), self.offsets_len) });
        let data = unsafe { slice::from_raw_parts(self.data_ptr, self.data_len) };

        let offset = *offsets
            .and_then(|v| v.get(self.next_offset_index))
            .ok_or(PayloadObjectReadError::Empty)?;
        let header_bytes = &data[offset..offset + size_of::<BinderObjectHeader>()];
        let header =
            unsafe { ptr::read_unaligned(header_bytes.as_ptr() as *const BinderObjectHeader) };
        if !matches!(header.type_, BinderType::FD) {
            return Err(PayloadObjectReadError::IncorrectObject);
        }

        let fd_bytes = &data[offset..offset + size_of::<BinderFdObject>()];
        let fd_obj = unsafe { ptr::read_unaligned(fd_bytes.as_ptr() as *const BinderFdObject) };
        let fd = unsafe { OwnedFd::from_raw_fd(fd_obj.fd) };
        self.next_offset_index += 1;
        self.next_data_index = offset + size_of::<BinderFdObject>();
        Ok((fd, fd_obj.cookie))
    }
    pub fn next_object_type(&self) -> Option<BinderObjectType> {
        let offsets = self
            .offsets_ptr
            .as_ref()
            .map(|ptr| unsafe { slice::from_raw_parts(ptr.as_ptr(), self.offsets_len) });
        let data = unsafe { slice::from_raw_parts(self.data_ptr, self.data_len) };

        let offset = *offsets.and_then(|v| v.get(self.next_offset_index))?;
        let header_bytes = &data[offset..offset + size_of::<BinderObjectHeader>()];
        let header =
            unsafe { ptr::read_unaligned(header_bytes.as_ptr() as *const BinderObjectHeader) };
        Some(match header.type_ {
            BinderType::BINDER => BinderObjectType::OwnedPort,
            BinderType::HANDLE => BinderObjectType::PortHandle,
            BinderType::WEAK_BINDER => BinderObjectType::WeakOwnedPort,
            BinderType::WEAK_HANDLE => BinderObjectType::WeakPortHandle,
            BinderType::FD => BinderObjectType::Fd,
            BinderType::FDA => BinderObjectType::FdArray,
            BinderType::PTR => BinderObjectType::Buffer,
            _ => return None,
        })
    }
    /// includes align bytes
    pub fn bytes_until_next_obj(&self) -> usize {
        let offsets = self
            .offsets_ptr
            .as_ref()
            .map(|ptr| unsafe { slice::from_raw_parts(ptr.as_ptr(), self.offsets_len) });
        let data = unsafe { slice::from_raw_parts(self.data_ptr, self.data_len) };

        let next_target = offsets
            .and_then(|v| v.iter().find(|v| **v > self.next_data_index).copied())
            .unwrap_or(data.len());
        next_target - self.next_data_index
    }
    // TODO: figure out how to do buffers, child buffers and fd arrays
}
pub enum BinderObjectType {
    OwnedPort,
    WeakOwnedPort,
    PortHandle,
    WeakPortHandle,
    Fd,
    FdArray,
    Buffer,
}
// should be fine?
unsafe impl Send for PayloadReader {}
unsafe impl Sync for PayloadReader {}
impl PayloadReader {
    pub(crate) unsafe fn from_raw(
        device: Arc<BinderDevice>,
        data_ptr: *const u8,
        data_len: usize,
        offsets_ptr: *const usize,
        offsets_len: usize,
    ) -> Self {
        Self {
            device,
            data_ptr,
            data_len,
            // not sure why i need to make the ptr mut
            offsets_ptr: NonNull::new(offsets_ptr as *mut _),
            offsets_len,
            next_offset_index: 0,
            next_data_index: 0,
        }
    }
}
impl Drop for PayloadReader {
    fn drop(&mut self) {
        unsafe {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&BinderCommand::FREE_BUFFER.as_u32().to_ne_bytes());
            bytes.extend_from_slice(&self.data_ptr.addr().to_ne_bytes());
            if let Some(addr) = self.offsets_ptr {
                bytes.extend_from_slice(&BinderCommand::FREE_BUFFER.as_u32().to_ne_bytes());
                bytes.extend_from_slice(&addr.addr().get().to_ne_bytes());
            }
            self.device.write_binder_command(&bytes);
        }
    }
}

#[derive(Error, Debug)]
pub enum PayloadPortReadError {
    #[error("Unexpected object type")]
    IncorrectObject,
    #[error("Unknown OwnedPort")]
    UnknownOwnedPort,
    #[error("No more ports to read")]
    Empty,
}
#[derive(Error, Debug)]
pub enum PayloadObjectReadError {
    #[error("Unexpected object type")]
    IncorrectObject,
    #[error("No more objects to read")]
    Empty,
}
#[derive(Error, Debug)]
pub enum PayloadBytesReadError {
    #[error("Tried reading bytes from an area containing an object")]
    ReadingIntoObject,
    #[error("Tried reading bytes outside the provided buffer")]
    OutOfBounds,
}
