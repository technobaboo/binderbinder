use crate::sys::{
    BinderFdObject, BinderObjectHeader, BinderType, FlatBinderFlags, FlatBinderObjectData,
    TransactionFlags,
};

use super::binder_ref::BinderRef;
use super::sys::{BinderTransactionData, BinderUintptrT, FlatBinderObject};
use byteorder::{LittleEndian, WriteBytesExt};
use std::io::{Cursor, Write};
use std::os::fd::RawFd;

pub const BINDER_MAX_TYPE: usize = 6;

pub enum BinderObjectEntry {
    Handle {
        handle: u32,
    },
    Fd {
        fd: RawFd,
    },
    Binder {
        ptr: BinderUintptrT,
        cookie: BinderUintptrT,
    },
}

impl BinderObjectEntry {
    pub fn from_flat(flat: &FlatBinderObject) -> Option<BinderObjectEntry> {
        match flat.hdr.type_ {
            BinderType::BINDER => Some(BinderObjectEntry::Binder {
                // the binder type should only be used locally
                ptr: unsafe { flat.data.binder },
                cookie: flat.cookie,
            }),
            // TODO: encode that this is weak somehow?
            BinderType::WEAK_BINDER => Some(BinderObjectEntry::Binder {
                // the binder type should only be used locally
                ptr: unsafe { flat.data.binder },
                cookie: flat.cookie,
            }),
            BinderType::HANDLE => Some(BinderObjectEntry::Handle {
                handle: unsafe { flat.data.handle },
            }),
            // TODO: encode that this is weak somehow?
            BinderType::WEAK_HANDLE => Some(BinderObjectEntry::Handle {
                handle: unsafe { flat.data.handle },
            }),
            BinderType::FD => Some(BinderObjectEntry::Fd {
                // TODO: i think this is correct?
                fd: unsafe {
                    (flat.data.binder as *const BinderFdObject)
                        .read_unaligned()
                        .fd as RawFd
                },
            }),
            // TODO: handle
            BinderType::FDA => None,
            BinderType::PTR => None,
            _ => None,
        }
    }

    pub fn to_flat(&self) -> FlatBinderObject {
        match self {
            BinderObjectEntry::Handle { handle } => FlatBinderObject {
                hdr: BinderObjectHeader {
                    type_: BinderType::HANDLE,
                },
                flags: FlatBinderFlags::empty(),
                binder: *handle as BinderUintptrT,
                cookie: 0,
            },
            BinderObjectEntry::Fd { fd } => FlatBinderObject {
                hdr: BinderObjectHeader {
                    type_: BinderType::FD,
                },
                flags: FlatBinderFlags::empty(),
                binder: *fd as BinderUintptrT,
                cookie: 0,
            },
            BinderObjectEntry::Binder { ptr, cookie } => FlatBinderObject {
                hdr: BinderObjectHeader {
                    type_: BinderType::BINDER,
                },
                flags: FlatBinderFlags::empty(),
                binder: *ptr,
                cookie: *cookie,
            },
        }
    }
}

#[derive(Default)]
pub struct Payload {
    pub data: Vec<u8>,
    pub objects: Vec<BinderObjectEntry>,
}
impl Payload {
    pub fn with_data(data: Vec<u8>) -> Self {
        Payload {
            data,
            objects: Vec::new(),
        }
    }

    pub fn add_binder(&mut self, ptr: BinderUintptrT, cookie: BinderUintptrT) {
        self.objects.push(BinderObjectEntry::Binder { ptr, cookie });
    }

    pub fn add_handle(&mut self, handle: u32) {
        self.objects.push(BinderObjectEntry::Handle { handle });
    }

    pub fn add_fd(&mut self, fd: i32) {
        self.objects.push(BinderObjectEntry::Fd { fd });
    }

    pub fn add_weak_binder(&mut self, ptr: BinderUintptrT, cookie: BinderUintptrT) {
        self.objects.push(BinderObjectEntry::Binder { ptr, cookie });
    }

    pub fn add_weak_handle(&mut self, handle: u32) {
        self.objects.push(BinderObjectEntry::Handle { handle });
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty() && self.objects.is_empty()
    }
}

pub struct Transaction {
    pub code: u32,
    pub payload: Payload,
}

pub enum BinderObject {
    Binder {
        ptr: BinderUintptrT,
        cookie: BinderUintptrT,
    },
    Handle {
        handle: u32,
    },
    Fd {
        fd: RawFd,
        fd_obj: BinderFdObject,
    },
    WeakBinder {
        ptr: BinderUintptrT,
        cookie: BinderUintptrT,
    },
    WeakHandle {
        handle: u32,
    },
}
impl BinderObject {
    pub fn to_flat(&self) -> FlatBinderObject {
        match self {
            BinderObject::Binder { ptr, cookie } => FlatBinderObject {
                hdr: BinderObjectHeader {
                    type_: BinderType::BINDER,
                },
                flags: FlatBinderFlags::empty(),
                data: unsafe { FlatBinderObjectData { binder: *ptr } },
                cookie: *cookie,
            },
            BinderObject::Handle { handle } => FlatBinderObject {
                hdr: BinderObjectHeader {
                    type_: BinderType::HANDLE,
                },
                flags: FlatBinderFlags::empty(),
                data: unsafe { FlatBinderObjectData { handle: *handle } },
                cookie: 0,
            },
            BinderObject::Fd { fd } => FlatBinderObject {
                hdr: BinderObjectHeader {
                    type_: BinderType::FD,
                },
                flags: FlatBinderFlags::empty(),
                binder: *fd as BinderUintptrT,
                cookie: 0,
            },
            BinderObject::WeakBinder { ptr, cookie } => {
                FlatBinderObject {
                    hdr: BinderObjectHeader {
                        type_: BinderType::WEAK_BINDER,
                    },
                    flags: FlatBinderFlags::empty(),
                    // TODO: is this correct?
                    binder: *ptr,
                    cookie: *cookie,
                }
            }
            BinderObject::WeakHandle { handle } => FlatBinderObject {
                hdr: BinderObjectHeader {
                    type_: BinderType::WEAK_HANDLE,
                },
                flags: FlatBinderFlags::empty(),
                binder: *handle as BinderUintptrT,
                cookie: 0,
            },
        }
    }
}

pub struct TransactionData {
    pub target: BinderRef,
    pub code: u32,
    pub flags: TransactionFlags,
    data: Vec<u8>,
    objects: Vec<BinderObject>,
}
impl TransactionData {
    pub fn new(target: BinderRef, code: u32, flags: TransactionFlags) -> Self {
        TransactionData {
            target,
            code,
            flags,
            data: Vec::new(),
            objects: Vec::new(),
        }
    }

    pub fn with_payload(mut self, payload: &[u8]) -> Self {
        self.data.extend_from_slice(payload);
        self
    }

    pub fn add_fd(mut self, fd: i32) -> Self {
        self.objects.push(BinderObject::Fd { fd });
        self
    }

    pub fn add_handle(mut self, handle: u32) -> Self {
        self.objects.push(BinderObject::Handle { handle });
        self
    }

    pub fn add_binder(mut self, ptr: BinderUintptrT, cookie: BinderUintptrT) -> Self {
        self.objects.push(BinderObject::Binder { ptr, cookie });
        self
    }

    pub fn add_weak_handle(mut self, handle: u32) -> Self {
        self.objects.push(BinderObject::WeakHandle { handle });
        self
    }

    pub fn add_weak_binder(mut self, ptr: BinderUintptrT, cookie: BinderUintptrT) -> Self {
        self.objects.push(BinderObject::WeakBinder { ptr, cookie });
        self
    }

    pub fn build(self) -> (Vec<u8>, Vec<FlatBinderObject>) {
        let flat_obj_size = std::mem::size_of::<FlatBinderObject>();
        eprintln!("DEBUG: flat_binder_object size = {}", flat_obj_size);
        eprintln!("DEBUG: data.len() = {}", self.data.len());
        eprintln!("DEBUG: objects.len() = {}", self.objects.len());

        let mut flat_objects = Vec::with_capacity(self.objects.len());
        let mut offsets = Vec::with_capacity(self.objects.len());

        for obj in self.objects {
            let flat = obj.to_flat();
            let offset = self.data.len() + offsets.len() * std::mem::size_of::<BinderUintptrT>();
            eprintln!("DEBUG: object {}, offset = {}", offsets.len(), offset);
            offsets.push(offset as BinderUintptrT);
            flat_objects.push(flat);
        }

        let mut tx_data = Vec::with_capacity(
            std::mem::size_of::<BinderTransactionData>()
                + self.data.len()
                + offsets.len() * std::mem::size_of::<BinderUintptrT>()
                + flat_objects.len() * std::mem::size_of::<FlatBinderObject>(),
        );

        eprintln!("DEBUG: tx_data capacity = {}", tx_data.capacity());
        eprintln!(
            "DEBUG: binder_transaction_data size = {}",
            std::mem::size_of::<BinderTransactionData>()
        );
        eprintln!("DEBUG: data.len() = {}", self.data.len());
        eprintln!("DEBUG: offsets.len() = {}", offsets.len());
        eprintln!("DEBUG: flat_objects.len() = {}", flat_objects.len());
        eprintln!(
            "DEBUG: flat_object size = {}",
            std::mem::size_of::<FlatBinderObject>()
        );
        eprintln!(
            "DEBUG: offset size = {}",
            std::mem::size_of::<BinderUintptrT>()
        );
        eprintln!(
            "DEBUG: expected total = {} + {} + {} * {} + {} * {} = {}",
            std::mem::size_of::<BinderTransactionData>(),
            self.data.len(),
            offsets.len(),
            std::mem::size_of::<BinderUintptrT>(),
            flat_objects.len(),
            std::mem::size_of::<FlatBinderObject>(),
            std::mem::size_of::<BinderTransactionData>()
                + self.data.len()
                + offsets.len() * std::mem::size_of::<BinderUintptrT>()
                + flat_objects.len() * std::mem::size_of::<FlatBinderObject>(),
        );

        eprintln!("DEBUG: tx_data capacity = {}", tx_data.capacity());
        eprintln!(
            "DEBUG: binder_transaction_data size = {}",
            std::mem::size_of::<BinderTransactionData>()
        );
        eprintln!("DEBUG: data.len() = {}", self.data.len());
        eprintln!("DEBUG: flat_objects.len() = {}", flat_objects.len());
        eprintln!(
            "DEBUG: flat_object size = {}",
            std::mem::size_of::<FlatBinderObject>()
        );
        eprintln!(
            "DEBUG: expected total = {} + {} + {} * {} = {}",
            std::mem::size_of::<BinderTransactionData>(),
            self.data.len(),
            flat_objects.len(),
            std::mem::size_of::<FlatBinderObject>(),
            std::mem::size_of::<BinderTransactionData>()
                + self.data.len()
                + flat_objects.len() * std::mem::size_of::<FlatBinderObject>(),
        );

        let mut cursor = Cursor::new(&mut tx_data);

        cursor
            .write_u64::<LittleEndian>(self.target.as_u32() as u64)
            .unwrap();
        cursor.write_u64::<LittleEndian>(0).unwrap();
        cursor.write_u32::<LittleEndian>(self.code).unwrap();
        cursor.write_u32::<LittleEndian>(self.flags.bits()).unwrap();
        cursor.write_u64::<LittleEndian>(0).unwrap();
        cursor.write_u64::<LittleEndian>(0).unwrap();
        cursor
            .write_u64::<LittleEndian>(self.data.len() as u64)
            .unwrap();
        cursor
            .write_u64::<LittleEndian>(offsets.len() as u64)
            .unwrap();

        cursor.write_all(&self.data).unwrap();

        for offset in offsets {
            cursor.write_u64::<LittleEndian>(offset).unwrap();
            eprintln!("DEBUG: writing offset {}", offset);
        }

        for (i, flat) in flat_objects.iter().enumerate() {
            let flat_bytes = unsafe {
                std::slice::from_raw_parts(
                    flat as *const FlatBinderObject as *const u8,
                    std::mem::size_of::<FlatBinderObject>(),
                )
            };
            cursor.write_all(flat_bytes).unwrap();
            eprintln!("DEBUG: writing flat object {}: {:02x?}", i, flat_bytes);
        }

        eprintln!("DEBUG: final tx_data.len() = {}", tx_data.len());
        eprintln!("DEBUG: tx_data as hex: {:02x?}", &tx_data);

        (tx_data, flat_objects)
    }

    pub fn parse_reply(data: &[u8]) -> (Vec<u8>, Vec<BinderObjectEntry>) {
        if data.len() < std::mem::size_of::<BinderTransactionData>() {
            return (Vec::new(), Vec::new());
        }

        let data_size_offset = 7 * 8;
        let offsets_size_offset = 8 * 8;
        let data_size = u64::from_le_bytes(
            data[data_size_offset..data_size_offset + 8]
                .try_into()
                .unwrap(),
        );
        let offsets_size = u64::from_le_bytes(
            data[offsets_size_offset..offsets_size_offset + 8]
                .try_into()
                .unwrap(),
        );

        let data_start = std::mem::size_of::<BinderTransactionData>();
        let offsets_start = data_start + data_size as usize;
        let _objects_start = offsets_start + offsets_size as usize;

        let reply_data = if data_size > 0 {
            data[data_start..offsets_start].to_vec()
        } else {
            Vec::new()
        };

        let mut objects = Vec::new();

        if offsets_size > 0 {
            let num_offsets = offsets_size as usize / std::mem::size_of::<BinderUintptrT>();

            for i in 0..num_offsets {
                let offset_offset = offsets_start + i * std::mem::size_of::<BinderUintptrT>();
                let offset =
                    u64::from_le_bytes(data[offset_offset..offset_offset + 8].try_into().unwrap());

                let obj_index = (offset as usize) / std::mem::size_of::<FlatBinderObject>();
                let obj_offset = data_start + obj_index * std::mem::size_of::<FlatBinderObject>();

                if obj_offset + std::mem::size_of::<FlatBinderObject>() <= data.len() {
                    let flat_ptr = data
                        [obj_offset..obj_offset + std::mem::size_of::<FlatBinderObject>()]
                        .as_ptr() as *const FlatBinderObject;
                    let flat = unsafe { &*flat_ptr };
                    if let Some(obj) = BinderObjectEntry::from_flat(flat) {
                        objects.push(obj);
                    }
                }
            }
        }

        (reply_data, objects)
    }

    /// Serialize flat_binder_objects to bytes for transmission.
    pub fn serialize_objects(objects: &[FlatBinderObject]) -> Vec<u8> {
        let mut result = Vec::with_capacity(std::mem::size_of_val(objects));
        for obj in objects {
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    obj as *const FlatBinderObject as *const u8,
                    std::mem::size_of::<FlatBinderObject>(),
                )
            };
            result.extend_from_slice(bytes);
        }
        result
    }
}

// pub struct FlatBinderObjectBuilder {
//     object: FlatBinderObject,
// }
//
// impl FlatBinderObjectBuilder {
//     pub fn new() -> Self {
//         FlatBinderObjectBuilder {
//             object: FlatBinderObject::default(),
//         }
//     }
//
//     pub fn binder(mut self, ptr: BinderUintptrT, cookie: BinderUintptrT) -> Self {
//         self.object.hdr.type_ = BINDER_TYPE_BINDER;
//         self.object.binder = ptr;
//         self.object.cookie = cookie;
//         self
//     }
//
//     pub fn handle(mut self, handle: u32) -> Self {
//         self.object.hdr.type_ = BINDER_TYPE_HANDLE;
//         self.object.binder = handle as BinderUintptrT;
//         self
//     }
//
//     pub fn fd(mut self, fd: i32) -> Self {
//         self.object.hdr.type_ = BINDER_TYPE_FD;
//         self.object.binder = fd as BinderUintptrT;
//         self
//     }
//
//     pub fn weak_binder(mut self, ptr: BinderUintptrT, cookie: BinderUintptrT) -> Self {
//         self.object.hdr.type_ = BINDER_TYPE_WEAK_BINDER;
//         self.object.binder = ptr;
//         self.object.cookie = cookie;
//         self
//     }
//
//     pub fn weak_handle(mut self, handle: u32) -> Self {
//         self.object.hdr.type_ = BINDER_TYPE_WEAK_HANDLE;
//         self.object.binder = handle as BinderUintptrT;
//         self
//     }
//
//     pub fn build(self) -> FlatBinderObject {
//         self.object
//     }
// }
//
// impl Default for FlatBinderObjectBuilder {
//     fn default() -> Self {
//         Self::new()
//     }
// }

#[test]
fn test_payload() {
    let mut payload = Payload::with_data(b"hello world".to_vec());
    payload.add_handle(42);
    payload.add_fd(123);
    payload.add_binder(0xDEADBEEF, 0xCAFEBABE);
    assert!(!payload.is_empty());
    assert_eq!(payload.data, b"hello world");
    assert_eq!(payload.objects.len(), 3);
}

#[test]
fn test_transaction_data() {
    let target = BinderRef::from_raw(123);
    let tx_data = TransactionData::new(target, 0x1234, TransactionFlags::ONE_WAY)
        .with_payload(b"hello")
        .add_handle(42);
    let (data, objects) = tx_data.build();
    assert!(!data.is_empty());
    assert_eq!(objects.len(), 1);
}
