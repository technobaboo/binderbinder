use std::fmt::{Debug, Formatter};
use std::os::fd::RawFd;

use bitflags::bitflags;
use rustix::ffi::c_void;
use rustix::ioctl::opcode::{none, read, write};
use rustix::ioctl::{opcode::read_write, Ioctl};
use rustix::process::{RawPid, RawUid};

pub type BinderSizeT = usize;
pub type BinderUintptrT = usize;

const fn b_pack_chars(c1: u8, c2: u8, c3: u8, c4: u8) -> u32 {
    ((c1 as u32) << 24) | ((c2 as u32) << 16) | ((c3 as u32) << 8) | (c4 as u32)
}
const B_TYPE_LARGE: u8 = 0x85;

macro_rules! non_exhaustive_enum {
    ($type:ident, $($name:ident = $value:expr,)*) => {
        #[derive(Clone, Copy, PartialEq, Eq)]
        #[repr(transparent)]
        pub struct $type(u32);
        impl $type {
            $(pub const $name: Self = Self($value);)*
        }
        impl Debug for $type {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                f.debug_tuple(stringify!($type)).field(match *self {
                    $(Self::$name => &stringify!($name),)*
                    _ => &self.0,
                }).finish()
            }
        }
    };
}
non_exhaustive_enum!(
    BinderType,
    BINDER = b_pack_chars(b's', b'b', b'*', B_TYPE_LARGE),
    WEAK_BINDER = b_pack_chars(b'w', b'b', b'*', B_TYPE_LARGE),
    HANDLE = b_pack_chars(b's', b'h', b'*', B_TYPE_LARGE),
    WEAK_HANDLE = b_pack_chars(b'w', b'h', b'*', B_TYPE_LARGE),
    FD = b_pack_chars(b'f', b'd', b'*', B_TYPE_LARGE),
    FDA = b_pack_chars(b'f', b'd', b'a', B_TYPE_LARGE),
    PTR = b_pack_chars(b'p', b't', b'*', B_TYPE_LARGE),
);

bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy)]
    pub struct FlatBinderFlags: u32 {
        const PRIORITY_MASK = 0xff;
        const ACCEPTS_FDS = 0x100;
        const SECURITY_CTX = 0x1000;
        const _ = !0;
    }
}

bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy)]
    pub struct BinderBufferFlags: u32 {
        const HAS_PARENT= 0x01;
        const _ = !0;
    }
}

pub const BINDER_CURRENT_PROTOCOL_VERSION: u32 = 8;

bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy)]
    pub struct TransactionFlags: u32 {
        const ONE_WAY = 0x01;
        const ROOT_OBJECT = 0x04;
        const STATUS_CODE = 0x08;
        const ACCEPT_FDS = 0x10;
        const CLEAR_BUF = 0x20;
        const UPDATE_TXN = 0x40;
        const _ = !0;
    }
}

non_exhaustive_enum!(
    BinderCommand,
    TRANSACTION = write::<BinderTransactionData>(b'c', 0),
    REPLY = write::<BinderTransactionData>(b'c', 1),
    ACQUIRE_RESULT = write::<i32>(b'c', 2),
    FREE_BUFFER = write::<BinderUintptrT>(b'c', 3),
    INCREFS = write::<u32>(b'c', 4),
    ACQUIRE = write::<u32>(b'c', 5),
    RELEASE = write::<u32>(b'c', 6),
    DECREFS = write::<u32>(b'c', 7),
    INCREFS_DONE = write::<BinderPtrCookie>(b'c', 8),
    ACQUIRE_DONE = write::<BinderPtrCookie>(b'c', 9),
    ATTEMPT_ACQUIRE = write::<BinderPriorityDesc>(b'c', 10),
    REGISTER_LOOPER = none(b'c', 11),
    ENTER_LOOPER = none(b'c', 12),
    EXIT_LOOPER = none(b'c', 13),
    REQUEST_DEATH_NOTIFICATION = write::<BinderHandleCookie>(b'c', 14),
    CLEAR_DEATH_NOTIFICATION = write::<BinderHandleCookie>(b'c', 15),
    DEAD_BINDER_DONE = write::<BinderUintptrT>(b'c', 16),
    TRANSACTION_SG = write::<BinderTransactionDataSg>(b'c', 17),
    REPLY_SG = write::<BinderTransactionDataSg>(b'c', 18),
    REQUEST_FREEZE_NOTIFICATION = write::<BinderHandleCookie>(b'c', 19),
    CLEAR_FREEZE_NOTIFICATION = write::<BinderHandleCookie>(b'c', 20),
    FREEZE_NOTIFICATION_DONE = write::<BinderUintptrT>(b'c', 21),
);
impl BinderCommand {
    pub fn as_u32(&self) -> u32 {
        self.0
    }
}

non_exhaustive_enum!(
    BinderReturn,
    ERROR = read::<i32>(b'r', 0),
    OK = none(b'r', 1),
    TRANSACTION_SEC_CTX = read::<BinderTransactionDataSecCtx>(b'r', 2),
    TRANSACTION = read::<BinderTransactionData>(b'r', 2),
    REPLY = read::<BinderTransactionData>(b'r', 3),
    ACQUIRE_RESULT = read::<i32>(b'r', 4),
    DEAD_REPLY = none(b'r', 5),
    TRANSACTION_COMPLETE = none(b'r', 6),
    INCREFS = read::<BinderPtrCookie>(b'r', 7),
    ACQUIRE = read::<BinderPtrCookie>(b'r', 8),
    RELEASE = read::<BinderPtrCookie>(b'r', 9),
    DECREFS = read::<BinderPtrCookie>(b'r', 10),
    ATTEMPT_ACQUIRE = read::<BinderPriorityPtrCookie>(b'r', 11),
    NOOP = none(b'r', 12),
    SPAWN_LOOPER = none(b'r', 13),
    FINISHED = none(b'r', 14),
    DEAD_BINDER = read::<BinderUintptrT>(b'r', 15),
    CLEAR_DEATH_NOTIFICATION_DONE = read::<BinderUintptrT>(b'r', 16),
    FAILED_REPLY = none(b'r', 17),
    FROZEN_REPLY = none(b'r', 18),
    ONEWAY_SPAM_SUSPECT = none(b'r', 19),
    TRANSACTION_PENDING_FROZEN = none(b'r', 20),
    FROZEN_BINDER = read::<BinderFrozenStateInfo>(b'r', 21),
    CLEAR_FREEZE_NOTIFICATION_DONE = read::<BinderUintptrT>(b'r', 22),
);
impl BinderReturn {
    pub fn from_u32(v: u32) -> Self {
        Self(v)
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BinderExtendedError {
    pub id: u32,
    pub command: u32,
    pub param: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BinderObjectHeader {
    pub type_: BinderType,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union FlatBinderObjectData {
    pub binder: BinderUintptrT,
    pub handle: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FlatBinderObject {
    pub hdr: BinderObjectHeader,
    pub flags: FlatBinderFlags,
    /// in the uapi this is flattened
    pub data: FlatBinderObjectData,
    pub cookie: BinderUintptrT,
}

impl Debug for FlatBinderObject {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlatBinderObject")
            .field("hdr", &self.hdr)
            .field("flags", &self.flags)
            .field(
                "data",
                match self.hdr.type_ {
                    BinderType::BINDER | BinderType::WEAK_BINDER => unsafe { &self.data.binder },
                    BinderType::HANDLE | BinderType::WEAK_HANDLE => unsafe { &self.data.handle },
                    _ => &"invalid binder type",
                },
            )
            .field("cookie", &self.cookie)
            .finish()
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union BinderFdObjectData {
    pub pad_binder: BinderUintptrT,
    pub fd: RawFd,
}

impl Debug for BinderFdObjectData {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BinderFdObjectData")
            // Safety: fd is always valid data and the other variant is only used for padding
            .field("fd", &unsafe { self.fd })
            .finish()
    }
}
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BinderFdObject {
    pub hdr: BinderObjectHeader,
    pub pad_flags: u32,
    /// in the uapi this is flattened
    pub data: BinderFdObjectData,
    pub cookie: BinderUintptrT,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BinderBufferObject {
    pub hdr: BinderObjectHeader,
    pub flags: BinderBufferFlags,
    pub buffer: BinderUintptrT,
    pub length: BinderSizeT,
    pub parent: BinderSizeT,
    pub parent_offset: BinderSizeT,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BinderFdArrayObject {
    pub hdr: BinderObjectHeader,
    pub _pad: u32,
    pub num_fds: BinderSizeT,
    pub parent: BinderSizeT,
    pub parent_offset: BinderSizeT,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct BinderWriteRead {
    pub write_size: BinderSizeT,
    pub write_consumed: BinderSizeT,
    pub write_buffer: BinderUintptrT,
    pub read_size: BinderSizeT,
    pub read_consumed: BinderSizeT,
    pub read_buffer: BinderUintptrT,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BinderVersion {
    pub protocol_version: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BinderTransactionDataPtrs {
    pub buffer: BinderUintptrT,
    pub offsets: BinderUintptrT,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union TransactionTarget {
    pub binder: BinderUintptrT,
    pub handle: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct BinderTransactionData {
    pub target: TransactionTarget,
    pub cookie: BinderUintptrT,
    pub code: u32,
    pub flags: TransactionFlags,
    pub sender_pid: RawPid,
    pub sender_euid: RawUid,
    pub data_size: BinderSizeT,
    pub offsets_size: BinderSizeT,
    pub data: BinderTransactionDataPtrs,
}

impl Debug for BinderTransactionData {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BinderTransactionData")
            .field("cookie", &self.cookie)
            .field("code", &self.code)
            .field("flags", &self.flags)
            .field("sender_pid", &self.sender_pid)
            .field("sender_euid", &self.sender_euid)
            .field("data_size", &self.data_size)
            .field("offsets_size", &self.offsets_size)
            .field("data", &self.data)
            .finish()
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BinderTransactionDataSecCtx {
    pub transaction_data: BinderTransactionData,
    pub sec_ctx: BinderUintptrT,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BinderTransactionDataSg {
    pub transaction_data: BinderTransactionData,
    pub buffer_size: BinderSizeT,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BinderPtrCookie {
    pub ptr: BinderUintptrT,
    pub cookie: BinderUintptrT,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BinderPriorityPtrCookie {
    pub priority: i32,
    pub ptr: BinderUintptrT,
    pub cookie: BinderUintptrT,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BinderHandleCookie {
    pub handle: u32,
    pub cookie: BinderUintptrT,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BinderfsDevice {
    pub name: [u8; 256],
    pub major: u32,
    pub minor: u32,
}

impl Default for BinderfsDevice {
    fn default() -> Self {
        BinderfsDevice {
            name: [0u8; 256],
            major: 0,
            minor: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BinderFrozenStateInfo {
    pub cookie: BinderUintptrT,
    pub is_frozen: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BinderPriorityDesc {
    priority: i32,
    desc: u32,
}

unsafe impl Ioctl for BinderVersion {
    type Output = BinderVersion;

    const IS_MUTATING: bool = true;

    fn opcode(&self) -> rustix::ioctl::Opcode {
        read_write::<BinderVersion>(b'b', 9)
    }

    fn as_ptr(&mut self) -> *mut c_void {
        self as *mut _ as *mut _
    }

    unsafe fn output_from_ptr(
        _out: rustix::ioctl::IoctlOutput,
        extract_output: *mut c_void,
    ) -> rustix::io::Result<Self::Output> {
        Ok(unsafe { *extract_output.cast::<Self>() })
    }
}

unsafe impl Ioctl for &mut BinderWriteRead {
    type Output = ();

    const IS_MUTATING: bool = true;

    fn opcode(&self) -> rustix::ioctl::Opcode {
        read_write::<BinderWriteRead>(b'b', 1)
    }

    fn as_ptr(&mut self) -> *mut c_void {
        *self as *mut BinderWriteRead as *mut _
    }

    unsafe fn output_from_ptr(
        _out: rustix::ioctl::IoctlOutput,
        _extract_output: *mut c_void,
    ) -> rustix::io::Result<Self::Output> {
        Ok(())
    }
}

unsafe impl Ioctl for BinderfsDevice {
    type Output = ();

    const IS_MUTATING: bool = true;

    fn opcode(&self) -> rustix::ioctl::Opcode {
        read_write::<Self>(b'b', 1)
    }

    fn as_ptr(&mut self) -> *mut c_void {
        self as *mut _ as *mut _
    }

    unsafe fn output_from_ptr(
        _out: rustix::ioctl::IoctlOutput,
        _extract_output: *mut c_void,
    ) -> rustix::io::Result<Self::Output> {
        Ok(())
    }
}

#[tokio::test]
async fn test_version_ioctl() {
    let file = std::fs::File::open("/dev/binderfs/testbinder").expect(
        "Could not open /dev/binderfs/testbinder. Run: sudo ./target/debug/examples/new_device",
    );
    let version = BinderVersion {
        protocol_version: 0,
    };
    let result = unsafe { rustix::ioctl::ioctl(&file, version) };
    assert!(result.is_ok());
    assert_eq!(result.unwrap().protocol_version, 8);
}
#[test]
pub const fn binder_type_niche() {
    let a = size_of::<BinderType>();
    let b = size_of::<Option<BinderType>>();
    if a == b {
        panic!("Binder Type has a niche");
    }
}
#[repr(transparent)]
pub struct SetContextMGR(pub FlatBinderObject);
unsafe impl Ioctl for SetContextMGR {
    type Output = ();

    const IS_MUTATING: bool = true;

    fn opcode(&self) -> rustix::ioctl::Opcode {
        write::<Self>(b'b', 13)
    }

    fn as_ptr(&mut self) -> *mut rustix::ffi::c_void {
        self as *mut _ as *mut _
    }

    unsafe fn output_from_ptr(
        _out: rustix::ioctl::IoctlOutput,
        _extract_output: *mut rustix::ffi::c_void,
    ) -> rustix::io::Result<Self::Output> {
        Ok(())
    }
}

#[repr(transparent)]
pub struct SetMaxThreads(pub u32);
unsafe impl Ioctl for SetMaxThreads {
    type Output = ();

    const IS_MUTATING: bool = true;

    fn opcode(&self) -> rustix::ioctl::Opcode {
        write::<Self>(b'b', 5)
    }

    fn as_ptr(&mut self) -> *mut rustix::ffi::c_void {
        self as *mut _ as *mut _
    }

    unsafe fn output_from_ptr(
        _out: rustix::ioctl::IoctlOutput,
        _extract_output: *mut rustix::ffi::c_void,
    ) -> rustix::io::Result<Self::Output> {
        Ok(())
    }
}

unsafe impl Ioctl for &mut BinderExtendedError {
    type Output = ();

    const IS_MUTATING: bool = true;

    fn opcode(&self) -> rustix::ioctl::Opcode {
        read_write::<BinderExtendedError>(b'b', 17)
    }

    fn as_ptr(&mut self) -> *mut rustix::ffi::c_void {
        *self as *mut BinderExtendedError as *mut _
    }

    unsafe fn output_from_ptr(
        _out: rustix::ioctl::IoctlOutput,
        _extract_output: *mut rustix::ffi::c_void,
    ) -> rustix::io::Result<Self::Output> {
        Ok(())
    }
}
