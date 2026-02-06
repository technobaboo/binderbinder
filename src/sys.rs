use std::marker::PhantomData;

use bitflags::bitflags;
use rustix::ffi::c_void;
use rustix::ioctl::opcode::{none, read, write};
use rustix::ioctl::{opcode::read_write, Ioctl};
use rustix::process::{RawPid, RawUid};

pub type BinderSizeT = u64;
pub type BinderUintptrT = u64;

/// TODO: value names in debug impl
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct BinderType(u32);
impl BinderType {
    pub const BINDER: Self = Self(0x17);
    pub const WEAK_BINDER: Self = Self(0x18);
    pub const HANDLE: Self = Self(0x19);
    pub const WEAK_HANDLE: Self = Self(0x1a);
    pub const FD: Self = Self(0x1b);
    /// Fd array
    pub const FDA: Self = Self(0x1c);
    pub const PTR: Self = Self(0x1d);
}

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

/// TODO: value names in debug impl
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct BinderCommand(u32);
impl BinderCommand {
    pub const TRANSACTION: Self = Self(write::<BinderTransactionData>(b'c', 0));
    pub const REPLY: Self = Self(write::<BinderTransactionData>(b'c', 1));
    pub const ACQUIRE_RESULT: Self = Self(write::<i32>(b'c', 2));
    pub const FREE_BUFFER: Self = Self(write::<BinderUintptrT>(b'c', 3));
    pub const INCREFS: Self = Self(write::<u32>(b'c', 4));
    pub const ACQUIRE: Self = Self(write::<u32>(b'c', 5));
    pub const RELEASE: Self = Self(write::<u32>(b'c', 6));
    pub const DECREFS: Self = Self(write::<u32>(b'c', 7));
    pub const INCREFS_DONE: Self = Self(write::<BinderPtrCookie>(b'c', 8));
    pub const ACQUIRE_DONE: Self = Self(write::<BinderPtrCookie>(b'c', 9));
    pub const ATTEMPT_ACQUIRE: Self = Self(write::<BinderPriorityDesc>(b'c', 10));
    pub const REGISTER_LOOPER: Self = Self(none(b'c', 11));
    pub const ENTER_LOOPER: Self = Self(none(b'c', 12));
    pub const EXIT_LOOPER: Self = Self(none(b'c', 13));
    pub const REQUEST_DEATH_NOTIFICATION: Self = Self(write::<BinderHandleCookie>(b'c', 14));
    pub const CLEAR_DEATH_NOTIFICATION: Self = Self(write::<BinderHandleCookie>(b'c', 15));
    pub const DEAD_BINDER_DONE: Self = Self(write::<BinderUintptrT>(b'c', 16));
    pub const TRANSACTION_SG: Self = Self(write::<BinderTransactionDataSg>(b'c', 17));
    pub const REPLY_SG: Self = Self(write::<BinderTransactionDataSg>(b'c', 18));
    pub const REQUEST_FREEZE_NOTIFICATION: Self = Self(write::<BinderHandleCookie>(b'c', 19));
    pub const CLEAR_FREEZE_NOTIFICATION: Self = Self(write::<BinderHandleCookie>(b'c', 20));
    pub const FREEZE_NOTIFICATION_DONE: Self = Self(write::<BinderUintptrT>(b'c', 21));

    pub fn as_u32(&self) -> u32 {
        self.0
    }
}

/// TODO: value names in debug impl
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct BinderReturn(u32);
impl BinderReturn {
    pub const ERROR: Self = Self(read::<i32>(b'r', 0));
    pub const OK: Self = Self(none(b'r', 1));
    pub const TRANSACTION_SEC_CTX: Self = Self(read::<BinderTransactionDataSecCtx>(b'r', 2));
    pub const TRANSACTION: Self = Self(read::<BinderTransactionData>(b'r', 2));
    pub const REPLY: Self = Self(read::<BinderTransactionData>(b'r', 3));
    pub const ACQUIRE_RESULT: Self = Self(read::<i32>(b'r', 4));
    pub const DEAD_REPLY: Self = Self(none(b'r', 5));
    pub const TRANSACTION_COMPLETE: Self = Self(none(b'r', 6));
    pub const INCREFS: Self = Self(read::<BinderPtrCookie>(b'r', 7));
    pub const ACQUIRE: Self = Self(read::<BinderPtrCookie>(b'r', 8));
    pub const RELEASE: Self = Self(read::<BinderPtrCookie>(b'r', 9));
    pub const DECREFS: Self = Self(read::<BinderPtrCookie>(b'r', 10));
    pub const ATTEMPT_ACQUIRE: Self = Self(read::<BinderPriorityPtrCookie>(b'r', 11));
    pub const NOOP: Self = Self(none(b'r', 12));
    pub const SPAWN_LOOPER: Self = Self(none(b'r', 13));
    pub const FINISHED: Self = Self(none(b'r', 14));
    pub const DEAD_BINDER: Self = Self(read::<BinderUintptrT>(b'r', 15));
    pub const CLEAR_DEATH_NOTIFICATION_DONE: Self = Self(read::<BinderUintptrT>(b'r', 16));
    pub const FAILED_REPLY: Self = Self(none(b'r', 17));
    pub const FROZEN_REPLY: Self = Self(none(b'r', 18));
    pub const ONEWAY_SPAM_SUSPECT: Self = Self(none(b'r', 19));
    pub const TRANSACTION_PENDING_FROZEN: Self = Self(none(b'r', 20));
    pub const FROZEN_BINDER: Self = Self(read::<BinderFrozenStateInfo>(b'r', 21));
    pub const CLEAR_FREEZE_NOTIFICATION_DONE: Self = Self(read::<BinderUintptrT>(b'r', 22));

    pub fn from_u32(v: u32) -> Self {
        Self(v)
    }
}

pub const BINDER_VERSION: u32 = 0x40046209;
pub const BINDER_SET_CONTEXT_MGR: u32 = 0x40046207;
pub const BINDER_SET_MAX_THREADS: u32 = 0x4004620c;
pub const BINDER_WRITE_READ: u32 = 0xc0306201;

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
pub struct FlatBinderObject<'a> {
    pub hdr: BinderObjectHeader,
    pub flags: FlatBinderFlags,
    /// in the uapi this is flattened
    pub data: FlatBinderObjectData,
    pub cookie: BinderUintptrT,
    pub _lifetime: PhantomData<&'a ()>,
}

impl<'a> std::fmt::Debug for FlatBinderObject<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlatBinderObject")
            .field("hdr", &self.hdr)
            .field("flags", &self.flags)
            .field(
                "data",
                match self.hdr.type_ {
                    // TODO: figure out if this is even correct, lol
                    BinderType::BINDER
                    | BinderType::FD
                    | BinderType::WEAK_BINDER
                    | BinderType::FDA
                    | BinderType::PTR => unsafe { &self.data.binder },
                    BinderType::HANDLE | BinderType::WEAK_HANDLE => unsafe { &self.data.handle },
                    _ => &"unknown",
                },
            )
            .field("cookie", &self.cookie)
            .finish()
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BinderFdObject {
    pub hdr: BinderObjectHeader,
    pub pad_flags: u32,
    pub pad_binder: BinderUintptrT,
    pub fd: u32,
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

impl std::fmt::Debug for BinderTransactionData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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
pub struct SetContextMGR<'a>(pub FlatBinderObject<'a>);
unsafe impl Ioctl for SetContextMGR<'_> {
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
