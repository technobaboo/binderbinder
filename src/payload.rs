use core::slice;
use std::{
    marker::PhantomData,
    ops::{Deref, Not},
    os::fd::{AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd},
    ptr::{self},
    sync::Arc,
};

use thiserror::Error;
use tracing::{debug, error, info};

use crate::{
    binder_ports::{BinderObjectId, BinderObjectOrRef, BinderRef, WeakBinderObject, WeakBinderRef},
    sys::{
        BinderBufferFlags, BinderBufferObject, BinderCommand, BinderFdArrayObject, BinderFdObject,
        BinderFdObjectData, BinderObjectHeader, BinderType, FlatBinderObject,
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
    pub fn push_port(&mut self, port: &BinderObjectOrRef) {
        self.align(align_of::<FlatBinderObject>());
        let flat_obj = port.get_flat_binder_object();
        debug!("pushing port: {port:?}");
        debug!("pushing port obj: {flat_obj:?}");
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
        debug!("pushing fd: {}", fd.as_raw_fd());
        let fd_obj = BinderFdObject {
            hdr: BinderObjectHeader {
                type_: BinderType::FD,
            },
            pad_flags: 0,
            data: BinderFdObjectData { fd: fd.as_raw_fd() },
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
        debug!("pushing fd: {}", fd.as_raw_fd());
        let fd_obj = BinderFdObject {
            hdr: BinderObjectHeader {
                type_: BinderType::FD,
            },
            pad_flags: 0,
            data: BinderFdObjectData { fd: fd.as_raw_fd() },
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
        while (self.data.as_ptr().addr() + self.data.len()) % align != 0 {
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
    pub(crate) fn fds(&self) -> &[OwnedFd] {
        &self.owned_fds
    }
}

enum PayloadReaderBuffer<T: Sized + 'static> {
    Owned(Vec<T>),
    KernelBorrowed(&'static [T]),
}
impl<T: Sized + 'static> Deref for PayloadReaderBuffer<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        match self {
            PayloadReaderBuffer::Owned(items) => items,
            PayloadReaderBuffer::KernelBorrowed(items) => items,
        }
    }
}

pub struct PayloadReader {
    device: Arc<BinderDevice>,
    data: PayloadReaderBuffer<u8>,
    offsets: Option<PayloadReaderBuffer<usize>>,

    next_offset_index: usize,
    next_data_index: usize,
}
impl PayloadReader {
    pub fn read_bytes<'a>(
        &'a mut self,
        num_bytes: usize,
    ) -> Result<&'a [u8], PayloadBytesReadError> {
        let offsets = self.offsets.as_mut();
        let data = &mut self.data;
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
    pub fn read_port(&mut self) -> Result<BinderObjectOrRef, PayloadPortReadError> {
        let offsets = self.offsets.as_mut();
        let data = &mut self.data;

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
            BinderType::BINDER => BinderObjectOrRef::Object(
                self.device
                    .owned_ports
                    .get(&BinderObjectId::from_raw(
                        unsafe { flat_obj.data.binder },
                        flat_obj.cookie,
                    ))
                    .ok_or(PayloadPortReadError::UnknownOwnedPort)?
                    .clone(),
            ),
            BinderType::WEAK_BINDER => BinderObjectOrRef::WeakObject(WeakBinderObject::from_id(
                self.device.clone(),
                BinderObjectId::from_raw(unsafe { flat_obj.data.binder }, flat_obj.cookie),
            )),
            BinderType::HANDLE => {
                BinderObjectOrRef::Ref(BinderRef::get_and_dedup_from_raw(&self.device, unsafe {
                    flat_obj.data.handle
                }))
            }
            BinderType::WEAK_HANDLE => BinderObjectOrRef::WeakRef(
                WeakBinderRef::get_and_dedup_from_raw(&self.device, unsafe {
                    flat_obj.data.handle
                }),
            ),
            _ => unreachable!("if this is ever reached, horrible things have happened"),
        };
        debug!("received object: {port:?}");
        self.next_offset_index += 1;
        self.next_data_index = offset + size_of::<FlatBinderObject>();
        Ok(port)
    }
    pub fn read_fd(&mut self) -> Result<(OwnedFd, usize), PayloadObjectReadError> {
        let offsets = self.offsets.as_mut();
        let data = &mut self.data;

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
        let fd = unsafe { OwnedFd::from_raw_fd(fd_obj.data.fd) };
        info!("received fd: {}", fd.as_raw_fd());
        self.next_offset_index += 1;
        self.next_data_index = offset + size_of::<BinderFdObject>();
        Ok((fd, fd_obj.cookie))
    }
    pub fn next_object_type(&self) -> Option<BinderObjectType> {
        let offsets = self.offsets.as_ref();
        let data = &self.data;

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
        let offsets = self.offsets.as_ref();
        let data = &self.data;

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
    pub(crate) unsafe fn from_kernel_raw(
        device: Arc<BinderDevice>,
        data_ptr: *const u8,
        data_len: usize,
        offsets_ptr: *const usize,
        offsets_len: usize,
    ) -> Self {
        Self {
            device,
            data: PayloadReaderBuffer::KernelBorrowed(unsafe {
                slice::from_raw_parts(data_ptr, data_len)
            }),
            // not sure why i need to make the ptr mut
            offsets: offsets_ptr.is_null().not().then(|| {
                PayloadReaderBuffer::KernelBorrowed(slice::from_raw_parts(offsets_ptr, offsets_len))
            }),
            next_offset_index: 0,
            next_data_index: 0,
        }
    }
    pub(crate) fn from_builder(device: Arc<BinderDevice>, builder: &PayloadBuilder) -> Self {
        let mut main_data = builder.data.clone();
        let offsets = builder.obj_offsets.clone();
        for offset in &offsets {
            let binder_type = {
                let header_bytes = &main_data[*offset..offset + size_of::<BinderObjectHeader>()];
                let header = unsafe {
                    ptr::read_unaligned(header_bytes.as_ptr() as *const BinderObjectHeader)
                };
                match header.type_ {
                    BinderType::BINDER
                    | BinderType::HANDLE
                    | BinderType::WEAK_BINDER
                    | BinderType::WEAK_HANDLE => continue,
                    BinderType::FD => BinderObjectType::Fd,
                    BinderType::FDA => todo!(),
                    BinderType::PTR => todo!(),
                    v => {
                        error!("unkown binder type: {v:?}");
                        continue;
                    }
                }
            };
            match binder_type {
                BinderObjectType::OwnedPort
                | BinderObjectType::WeakOwnedPort
                | BinderObjectType::PortHandle
                | BinderObjectType::WeakPortHandle => unreachable!(),
                BinderObjectType::Fd => {
                    let fd_bytes = &mut main_data[*offset..offset + size_of::<BinderFdObject>()];
                    let fd_obj = fd_bytes.as_mut_ptr() as *mut BinderFdObject;
                    let fd_obj = unsafe { fd_obj.as_mut().unwrap() };
                    let fd = unsafe { fd_obj.data.fd };
                    let new_fd = unsafe { BorrowedFd::borrow_raw(fd) }
                        .try_clone_to_owned()
                        .unwrap();
                    fd_obj.data = BinderFdObjectData {
                        fd: new_fd.into_raw_fd(),
                    }
                }
                BinderObjectType::FdArray => todo!(),
                BinderObjectType::Buffer => todo!(),
            }
        }
        Self {
            device,
            data: PayloadReaderBuffer::Owned(main_data),
            offsets: Some(PayloadReaderBuffer::Owned(offsets)),
            next_offset_index: 0,
            next_data_index: 0,
        }
    }
}
impl<T: Sized + 'static> PayloadReaderBuffer<T> {
    unsafe fn free(&mut self, device: &Arc<BinderDevice>) {
        if let PayloadReaderBuffer::KernelBorrowed(items) = self {
            // TODO: figure out if this causes mem leaks, should be pretty simple
            unsafe {
                let mut bytes = Vec::new();
                bytes.extend_from_slice(&BinderCommand::FREE_BUFFER.as_u32().to_ne_bytes());
                bytes.extend_from_slice(&(items.as_ptr() as usize).to_ne_bytes());
                device.write_binder_command(&bytes);
            }
        }
    }
}
impl Drop for PayloadReader {
    fn drop(&mut self) {
        unsafe {
            self.data.free(&self.device);
            self.offsets.as_mut().map(|v| v.free(&self.device));
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
