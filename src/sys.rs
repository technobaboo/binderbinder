use rustix::ffi::c_void;
use rustix::ioctl::{opcode::read_write, Ioctl};

pub type binder_size_t = u64;
pub type binder_uintptr_t = u64;

pub const BINDER_TYPE_BINDER: u32 = 0x17;
pub const BINDER_TYPE_WEAK_BINDER: u32 = 0x18;
pub const BINDER_TYPE_HANDLE: u32 = 0x19;
pub const BINDER_TYPE_WEAK_HANDLE: u32 = 0x1a;
pub const BINDER_TYPE_FD: u32 = 0x1b;
pub const BINDER_TYPE_FDA: u32 = 0x1c;
pub const BINDER_TYPE_PTR: u32 = 0x1d;

pub const FLAT_BINDER_FLAG_PRIORITY_MASK: u32 = 0xff;
pub const FLAT_BINDER_FLAG_ACCEPTS_FDS: u32 = 0x100;
pub const FLAT_BINDER_FLAG_TXN_SECURITY_CTX: u32 = 0x1000;

pub const BINDER_BUFFER_FLAG_HAS_PARENT: u32 = 0x01;

pub const BINDER_CURRENT_PROTOCOL_VERSION: u32 = 8;

pub const TF_ONE_WAY: u32 = 0x01;
pub const TF_ROOT_OBJECT: u32 = 0x04;
pub const TF_STATUS_CODE: u32 = 0x08;
pub const TF_ACCEPT_FDS: u32 = 0x10;
pub const TF_CLEAR_BUF: u32 = 0x20;
pub const TF_UPDATE_TXN: u32 = 0x40;

pub const BC_TRANSACTION: u32 = 0xc0306200;
pub const BC_REPLY: u32 = 0xc0306201;
pub const BC_ACQUIRE_RESULT: u32 = 0xc0306202;
pub const BC_FREE_BUFFER: u32 = 0xc0306300;
pub const BC_INCREFS: u32 = 0xc0306404;
pub const BC_ACQUIRE: u32 = 0xc0306405;
pub const BC_RELEASE: u32 = 0xc0306406;
pub const BC_DECREFS: u32 = 0xc0306407;
pub const BC_INCREFS_DONE: u32 = 0xc0306508;
pub const BC_ACQUIRE_DONE: u32 = 0xc0306509;
pub const BC_ATTEMPT_ACQUIRE: u32 = 0xc030650a;
pub const BC_REGISTER_LOOPER: u32 = 0xc030660b;
pub const BC_ENTER_LOOPER: u32 = 0xc030660c;
pub const BC_EXIT_LOOPER: u32 = 0xc030660d;
pub const BC_REQUEST_DEATH_NOTIFICATION: u32 = 0xc030670e;
pub const BC_CLEAR_DEATH_NOTIFICATION: u32 = 0xc030670f;
pub const BC_DEAD_BINDER_DONE: u32 = 0xc0306810;
pub const BC_TRANSACTION_SG: u32 = 0xc0306213;
pub const BC_REPLY_SG: u32 = 0xc0306214;

pub const BR_ERROR: u32 = 0x72080000;
pub const BR_OK: u32 = 0x72080001;
pub const BR_TRANSACTION: u32 = 0x72080002;
pub const BR_REPLY: u32 = 0x72080003;
pub const BR_ACQUIRE_RESULT: u32 = 0x72080004;
pub const BR_DEAD_REPLY: u32 = 0x72080005;
pub const BR_TRANSACTION_COMPLETE: u32 = 0x72080006;
pub const BR_INCREFS: u32 = 0x72080007;
pub const BR_ACQUIRE: u32 = 0x72080008;
pub const BR_RELEASE: u32 = 0x72080009;
pub const BR_DECREFS: u32 = 0x7208000a;
pub const BR_ATTEMPT_ACQUIRE: u32 = 0x7208000b;
pub const BR_NOOP: u32 = 0x7208000c;
pub const BR_SPAWN_LOOPER: u32 = 0x7208000d;
pub const BR_FINISHED: u32 = 0x7208000e;
pub const BR_DEAD_BINDER: u32 = 0x7208000f;
pub const BR_CLEAR_DEATH_NOTIFICATION_DONE: u32 = 0x72080010;
pub const BR_FAILED_REPLY: u32 = 0x72080011;
pub const BR_FROZEN_REPLY: u32 = 0x72080012;
pub const BR_ONEWAY_SPAM_SUSPECT: u32 = 0x72080013;
pub const BR_TRANSACTION_PENDING_FROZEN: u32 = 0x72080014;
pub const BR_FROZEN_BINDER: u32 = 0x72080015;
pub const BR_CLEAR_FREEZE_NOTIFICATION_DONE: u32 = 0x72080016;

pub const BINDER_VERSION: u32 = 0x40046209;
pub const BINDER_SET_CONTEXT_MGR: u32 = 0x40046207;
pub const BINDER_SET_MAX_THREADS: u32 = 0x4004620c;
pub const BINDER_WRITE_READ: u32 = 0xc0306201;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct binder_object_header {
    pub type_: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct flat_binder_object {
    pub hdr: binder_object_header,
    pub flags: u32,
    pub binder: binder_uintptr_t,
    pub cookie: binder_uintptr_t,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct binder_fd_object {
    pub hdr: binder_object_header,
    pub pad_flags: u32,
    pub pad_binder: binder_uintptr_t,
    pub fd: u32,
    pub cookie: binder_uintptr_t,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct binder_buffer_object {
    pub hdr: binder_object_header,
    pub flags: u32,
    pub buffer: binder_uintptr_t,
    pub length: binder_size_t,
    pub parent: binder_size_t,
    pub parent_offset: binder_size_t,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct binder_fd_array_object {
    pub hdr: binder_object_header,
    pub pad: u32,
    pub num_fds: binder_size_t,
    pub parent: binder_size_t,
    pub parent_offset: binder_size_t,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct binder_write_read {
    pub write_size: binder_size_t,
    pub write_consumed: binder_size_t,
    pub write_buffer: binder_uintptr_t,
    pub read_size: binder_size_t,
    pub read_consumed: binder_size_t,
    pub read_buffer: binder_uintptr_t,
}

impl Default for binder_write_read {
    fn default() -> Self {
        binder_write_read {
            write_size: 0,
            write_consumed: 0,
            write_buffer: 0,
            read_size: 0,
            read_consumed: 0,
            read_buffer: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct binder_version {
    pub protocol_version: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct binder_transaction_data_ptrs {
    pub buffer: binder_uintptr_t,
    pub offsets: binder_uintptr_t,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct binder_transaction_data {
    pub target: binder_uintptr_t,
    pub cookie: binder_uintptr_t,
    pub code: u32,
    pub flags: u32,
    pub sender_pid: i32,
    pub sender_euid: i32,
    pub data_size: binder_size_t,
    pub offsets_size: binder_size_t,
    pub data: binder_transaction_data_ptrs,
}

impl Default for binder_transaction_data {
    fn default() -> Self {
        binder_transaction_data {
            target: 0,
            cookie: 0,
            code: 0,
            flags: 0,
            sender_pid: 0,
            sender_euid: 0,
            data_size: 0,
            offsets_size: 0,
            data: binder_transaction_data_ptrs {
                buffer: 0,
                offsets: 0,
            },
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct binder_ptr_cookie {
    pub ptr: binder_uintptr_t,
    pub cookie: binder_uintptr_t,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct binder_handle_cookie {
    pub handle: u32,
    pub cookie: binder_uintptr_t,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct binderfs_device {
    pub name: [u8; 256],
    pub major: u32,
    pub minor: u32,
}

impl Default for binderfs_device {
    fn default() -> Self {
        binderfs_device {
            name: [0u8; 256],
            major: 0,
            minor: 0,
        }
    }
}

unsafe impl Ioctl for binder_version {
    type Output = binder_version;

    const IS_MUTATING: bool = true;

    fn opcode(&self) -> rustix::ioctl::Opcode {
        read_write::<binder_version>(b'b', 9)
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

unsafe impl Ioctl for binder_write_read {
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

unsafe impl Ioctl for binderfs_device {
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

use crate::*;

#[tokio::test]
async fn test_version_ioctl() {
    let file = std::fs::File::open("/dev/binderfs/testbinder").expect(
        "Could not open /dev/binderfs/testbinder. Run: sudo ./target/debug/examples/new_device",
    );
    let version = binder_version {
        protocol_version: 0,
    };
    let result = unsafe { rustix::ioctl::ioctl(&file, version) };
    assert!(result.is_ok());
    assert_eq!(result.unwrap().protocol_version, 8);
}
