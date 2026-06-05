use std::io;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("OS error: {0}")]
    Os(i32),
    #[error("binder operation failed: {0}")]
    Binder(#[from] rustix::io::Errno),
    #[error("invalid handle: {0}")]
    InvalidHandle(u32),
    #[error("handle not found: {0}")]
    HandleNotFound(u32),
    #[error("invalid transaction data")]
    InvalidTransaction,
    #[error("transaction failed: {0}")]
    Transaction(String),
    #[error("object not found")]
    ObjectNotFound,
    #[error("invalid object type")]
    InvalidObjectType,
    #[error("not connected to binder driver")]
    NotConnected,
    #[error("already connected")]
    AlreadyConnected,
    #[error("permission denied")]
    PermissionDenied,
    #[error("out of memory")]
    OutOfMemory,
    #[error("invalid argument")]
    InvalidArgument,
    #[error("actor shutdown")]
    Shutdown,
    #[error("channel full")]
    ChannelFull,
    #[error("dead binder")]
    DeadBinder,
    #[error("dead reply")]
    DeadReply,
    #[error("already exists")]
    AlreadyExists,
    #[error("unknown error: {0}")]
    Unknown(i32),
}

pub type Result<T> = std::result::Result<T, Error>;
