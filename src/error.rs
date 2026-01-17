use std::fmt;
use std::io;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Os(i32),
    Binder(rustix::io::Errno),
    InvalidHandle(u32),
    HandleNotFound(u32),
    InvalidTransaction,
    Transaction(String),
    ObjectNotFound,
    InvalidObjectType,
    NotConnected,
    AlreadyConnected,
    PermissionDenied,
    OutOfMemory,
    InvalidArgument,
    Shutdown,
    ChannelFull,
    DeadBinder,
    DeadReply,
    AlreadyExists,
    Unknown(i32),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "IO error: {}", e),
            Error::Os(e) => write!(f, "OS error: {}", e),
            Error::Binder(e) => write!(f, "binder operation failed: {}", e),
            Error::InvalidHandle(h) => write!(f, "invalid handle: {}", h),
            Error::HandleNotFound(h) => write!(f, "handle not found: {}", h),
            Error::InvalidTransaction => write!(f, "invalid transaction data"),
            Error::Transaction(msg) => write!(f, "transaction failed: {}", msg),
            Error::ObjectNotFound => write!(f, "object not found"),
            Error::InvalidObjectType => write!(f, "invalid object type"),
            Error::NotConnected => write!(f, "not connected to binder driver"),
            Error::AlreadyConnected => write!(f, "already connected"),
            Error::PermissionDenied => write!(f, "permission denied"),
            Error::OutOfMemory => write!(f, "out of memory"),
            Error::InvalidArgument => write!(f, "invalid argument"),
            Error::Shutdown => write!(f, "actor shutdown"),
            Error::ChannelFull => write!(f, "channel full"),
            Error::DeadBinder => write!(f, "dead binder"),
            Error::DeadReply => write!(f, "dead reply"),
            Error::AlreadyExists => write!(f, "already exists"),
            Error::Unknown(e) => write!(f, "unknown error: {}", e),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Io(err)
    }
}

impl From<i32> for Error {
    fn from(errno: i32) -> Self {
        match errno {
            0 => unreachable!(),
            libc::EBADF => Error::InvalidArgument,
            libc::ENOMEM => Error::OutOfMemory,
            libc::EACCES => Error::PermissionDenied,
            libc::EINVAL => Error::InvalidArgument,
            libc::ENOENT => Error::HandleNotFound(0),
            libc::EBUSY => Error::AlreadyConnected,
            _ => Error::Unknown(errno),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
