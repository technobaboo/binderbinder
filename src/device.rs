use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, IntoRawFd, OwnedFd};
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

use crate::sys::SetContextMGR;

use super::binder_ref::BinderRef;
use super::error::{Error, Result};
use super::sys::{
    binder_transaction_data, binder_write_read, flat_binder_object, BinderSizeT, BinderUintptrT,
    BC_REPLY, BC_TRANSACTION, BINDER_SET_CONTEXT_MGR, BINDER_TYPE_BINDER, BINDER_WRITE_READ,
    BR_DEAD_REPLY, BR_NOOP, BR_REPLY, BR_TRANSACTION, BR_TRANSACTION_COMPLETE, TF_ONE_WAY,
};
use super::transaction::{Payload, Transaction, TransactionData};

enum ActorMessage {
    TransactTwoWay {
        target: BinderRef,
        code: u32,
        payload: Payload,
        reply_tx: oneshot::Sender<Result<Payload>>,
    },
    TransactOneWay {
        target: BinderRef,
        code: u32,
        payload: Payload,
    },
    Register {
        cookie: BinderUintptrT,
        tx: mpsc::Sender<Transaction>,
    },
    Unregister {
        cookie: BinderUintptrT,
    },
    SetContextManager {
        obj: BinderObject,
        reply_tx: oneshot::Sender<Result<BinderRef>>,
    },
    Reply {
        payload: Payload,
        reply_tx: oneshot::Sender<()>,
    },
    Shutdown,
}

struct PendingReply {
    tx: oneshot::Sender<Result<Payload>>,
    target: BinderRef,
}

pub struct BinderObject {
    pub cookie: BinderUintptrT,
    rx: mpsc::Receiver<Transaction>,
}

impl BinderObject {
    pub fn cookie(&self) -> BinderUintptrT {
        self.cookie
    }

    pub async fn recv_transaction(&mut self) -> Option<Transaction> {
        self.rx.recv().await
    }
}

pub struct BinderDevice {
    cookie_counter: AtomicU64,
    msg_tx: mpsc::Sender<ActorMessage>,
    task: AbortHandle,
}

impl BinderDevice {
    pub async fn open(file: File) -> Result<Self> {
        let fd = OwnedFd::from(file);

        let (msg_tx, msg_rx) = mpsc::channel(32);

        let task = tokio::task::spawn_local(run_actor(fd, msg_rx)).abort_handle();

        let device = BinderDevice {
            cookie_counter: AtomicU64::new(1),
            msg_tx,
            task,
        };

        Ok(device)
    }

    pub async fn transact(
        &self,
        target: BinderRef,
        code: u32,
        payload: Payload,
    ) -> Result<Payload> {
        let (reply_tx, reply_rx) = oneshot::channel();

        self.msg_tx
            .send(ActorMessage::TransactTwoWay {
                target,
                code,
                payload,
                reply_tx,
            })
            .await
            .map_err(|_| Error::Shutdown)?;

        reply_rx.await.map_err(|_| Error::Shutdown)?
    }

    pub fn transact_no_reply(&self, target: BinderRef, code: u32, payload: Payload) -> Result<()> {
        self.msg_tx
            .try_send(ActorMessage::TransactOneWay {
                target,
                code,
                payload,
            })
            .map_err(|_| Error::ChannelFull)
    }

    pub fn register_object(&self) -> BinderObject {
        let cookie = self.cookie_counter.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(32);

        self.msg_tx
            .try_send(ActorMessage::Register { cookie, tx })
            .unwrap();

        BinderObject { cookie, rx }
    }

    pub async fn set_context_manager(&self, obj: BinderObject) -> Result<BinderRef> {
        let (reply_tx, reply_rx) = oneshot::channel();

        self.msg_tx
            .send(ActorMessage::SetContextManager { obj, reply_tx })
            .await
            .map_err(|_| Error::Shutdown)?;

        reply_rx.await.map_err(|_| Error::Shutdown)?
    }
}

impl Drop for BinderDevice {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run_actor(fd: OwnedFd, mut msg_rx: mpsc::Receiver<ActorMessage>) {
    eprintln!("Creating AsyncFd for fd={}", fd.as_raw_fd());
    let async_fd = match AsyncFd::new(fd) {
        Ok(fd) => {
            eprintln!("AsyncFd created successfully");
            fd
        }
        Err(e) => {
            eprintln!("Failed to create AsyncFd for binder: {}", e);
            return;
        }
    };

    let read_buf_size = 256 * 1024;
    let mut read_buf = vec![0u8; read_buf_size];
    eprintln!("Read buffer allocated: {} bytes", read_buf_size);

    let mut registry: HashMap<BinderUintptrT, mpsc::Sender<Transaction>> = HashMap::new();
    let mut pending_replies: VecDeque<PendingReply> = VecDeque::new();
    let mut context_manager_set = false;
    let mut shutdown = false;

    eprintln!("Entering main actor loop...");
    while !shutdown {
        tokio::select! {
            msg = msg_rx.recv() => {
                match msg {
                    Some(ActorMessage::TransactTwoWay { target, code, payload, reply_tx }) => {
                        eprintln!("TransactTwoWay: target={:?}, code={}", target, code);
                        let tx_data = TransactionData::new(target, code, 0);
                        let (data, _objects) = tx_data.with_payload(&payload.data).build();

                        send_transaction_sync(async_fd.get_ref().as_fd(), &data);

                        let pending = PendingReply {
                            tx: reply_tx,
                            target,
                        };
                        pending_replies.push_back(pending);
                    }
                    Some(ActorMessage::TransactOneWay { target, code, payload }) => {
                        eprintln!("TransactOneWay: target={:?}, code={}", target, code);
                        handle_transact_one_way(async_fd.get_ref().as_fd(), target, code, payload).await;
                    }
                    Some(ActorMessage::Register { cookie, tx }) => {
                        eprintln!("Register: cookie=0x{:x}", cookie);
                        registry.insert(cookie, tx);
                    }
                    Some(ActorMessage::Unregister { cookie }) => {
                        eprintln!("Unregister: cookie=0x{:x}", cookie);
                        registry.remove(&cookie);
                    }
                    Some(ActorMessage::SetContextManager { obj: _, reply_tx }) => {
                        eprintln!("SetContextManager");
                        if context_manager_set {
                            let _ = reply_tx.send(Err(Error::AlreadyExists));
                            continue;
                        }
                        match set_context_manager(async_fd.get_ref().as_fd()).await {
                            Ok(()) => {
                                context_manager_set = true;
                                let _ = reply_tx.send(Ok(BinderRef(0)));
                            }
                            Err(e) => {
                                eprintln!("set_context_manager failed: {:?}", e);
                                let _ = reply_tx.send(Err(e));
                            }
                        }
                    }
                    Some(ActorMessage::Reply { payload, reply_tx: _ }) => {
                        eprintln!("Reply: payload.len={}", payload.data.len());
                        send_bc_reply(async_fd.get_ref().as_fd(), &payload);
                    }
                    Some(ActorMessage::Shutdown) | None => {
                        eprintln!("Shutdown received");
                        shutdown = true;
                    }
                }
            }
            Ok(mut guard) = async_fd.readable() => {
                match guard.try_io(|inner| {
                    let wr = binder_write_read {
                        write_size: 0,
                        write_consumed: 0,
                        write_buffer: 0,
                        read_size: read_buf_size as BinderSizeT,
                        read_consumed: 0,
                        read_buffer: read_buf.as_mut_ptr() as BinderUintptrT,
                    };

                    let res = unsafe {
                        rustix::ioctl::ioctl(
                            rustix::fd::BorrowedFd::borrow_raw(inner.as_raw_fd()),
                            wr,
                        )
                    };

                    match res {
                        Ok(_) => {
                            let consumed = wr.read_consumed as usize;
                            let commands: Vec<u32> = if consumed >= 4 {
                                let num_cmds = consumed / 4;
                                let ptr = read_buf.as_ptr() as *const u32;

                                (0..num_cmds)
                                    .map(|i| unsafe { *ptr.add(i) })
                                    .collect()
                            } else {
                                Vec::new()
                            };

                            Ok((wr, commands))
                        }
                            Err(_) => Err(std::io::Error::other("ioctl failed")),
                    }
                }) {
                    Ok(Ok((wr, commands))) => {
                        eprintln!("Processing {} commands", commands.len());
                        for cmd in &commands {
                            match *cmd {
                                BR_NOOP => { eprintln!("BR_NOOP"); }
                                BR_TRANSACTION => {
                                    eprintln!("BR_TRANSACTION received!");
                                    if let Some(tx) = parse_br_transaction(async_fd.get_ref().as_fd(), &wr, &commands) {
                                        let cookie = extract_cookie(&wr);
                                        eprintln!("  cookie=0x{:x}", cookie);
                                        if let Some(object_tx) = registry.get(&cookie) {
                                            eprintln!("  Sending to registered object");
                                            let _ = object_tx.try_send(tx);
                                        } else {
                                            eprintln!("  No object registered for cookie=0x{:x}", cookie);
                                        }
                                    }
                                }
                                BR_TRANSACTION_COMPLETE => { eprintln!("BR_TRANSACTION_COMPLETE"); }
                                BR_REPLY => {
                                    eprintln!("BR_REPLY");
                                    handle_br_reply(&mut pending_replies, &wr);
                                }
                                BR_DEAD_REPLY => {
                                    eprintln!("BR_DEAD_REPLY");
                                    handle_br_dead_reply(&mut pending_replies);
                                }
                                BR_SPAWN_LOOPER => {
                                    eprintln!("BR_SPAWN_LOOPER - continuing");
                                }
                                _ => {
                                    eprintln!("Unhandled binder command: {:x}", cmd);
                                }
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        eprintln!("try_io error: {:?}", e);
                        if e.kind() == std::io::ErrorKind::WouldBlock {
                            guard.clear_ready();
                        }
                    }
                    Err(_) => {
                        eprintln!("try_io error - clearing ready");
                        guard.clear_ready();
                    }
                }
            }
        }
    }

    eprintln!("Actor loop exiting, shutting down...");
    shutdown_actor(&mut pending_replies, &mut registry);
}

async fn handle_transact_one_way(
    fd: BorrowedFd<'_>,
    target: BinderRef,
    code: u32,
    payload: Payload,
) {
    let tx_data = TransactionData::new(target, code, TF_ONE_WAY);
    let (data, _objects) = tx_data.with_payload(&payload.data).build();

    send_transaction_sync(fd, &data);
}

fn send_transaction_sync(fd: BorrowedFd<'_>, data: &[u8]) {
    let wr = binder_write_read {
        write_buffer: data.as_ptr() as BinderUintptrT,
        write_size: data.len() as BinderSizeT,
        write_consumed: 0,
        read_buffer: 0,
        read_size: 0,
        read_consumed: 0,
    };

    let res = unsafe { rustix::ioctl::ioctl(fd, wr) };

    if res.is_err() {
        let err = std::io::Error::last_os_error();
        eprintln!("send_transaction error: {:?}", err);
    }
}

fn send_binder_transaction(fd: RawFd, data: &[u8]) -> Result<()> {
    eprintln!("send_binder_transaction: {} bytes", data.len());
    eprintln!("  BC_TRANSACTION = 0x{:x}", BC_TRANSACTION);
    eprintln!(
        "  transaction_data size = {}",
        std::mem::size_of::<binder_transaction_data>()
    );
    eprintln!("  data as hex: {:02x?}", data);

    let mut write_buf = Vec::with_capacity(4 + data.len());
    write_buf.extend_from_slice(&BC_TRANSACTION.to_le_bytes());
    write_buf.extend_from_slice(data);

    eprintln!("  write_buf size = {}", write_buf.len());
    eprintln!(
        "  write_buf[0..4] (cmd) = {:02x?}",
        &write_buf[..std::cmp::min(4, write_buf.len())]
    );
    eprintln!(
        "  write_buf[4..12] (target.handle) = {:02x?}",
        &write_buf[4..std::cmp::min(12, write_buf.len())]
    );
    eprintln!(
        "  write_buf[12..20] (cookie) = {:02x?}",
        &write_buf[12..std::cmp::min(20, write_buf.len())]
    );
    eprintln!(
        "  write_buf[20..24] (code) = {:02x?}",
        &write_buf[20..std::cmp::min(24, write_buf.len())]
    );
    eprintln!(
        "  write_buf[24..28] (flags) = {:02x?}",
        &write_buf[24..std::cmp::min(28, write_buf.len())]
    );
    eprintln!(
        "  write_buf[56..64] (data_size, offsets_size) = {:02x?}",
        &write_buf[56..std::cmp::min(64, write_buf.len())]
    );
    eprintln!(
        "  write_buf[64..80] (data) = {:02x?}",
        &write_buf[64..std::cmp::min(80, write_buf.len())]
    );
    eprintln!(
        "  write_buf[80..88] (offset) = {:02x?}",
        &write_buf[80..std::cmp::min(88, write_buf.len())]
    );

    let mut wr = binder_write_read {
        write_buffer: write_buf.as_ptr() as BinderUintptrT,
        write_size: write_buf.len() as BinderSizeT,
        write_consumed: 0,
        read_buffer: 0,
        read_size: 0,
        read_consumed: 0,
    };

    eprintln!(
        "  calling ioctl with BINDER_WRITE_READ=0x{:x}",
        BINDER_WRITE_READ
    );
    let res = unsafe { rustix::ioctl::ioctl(rustix::fd::BorrowedFd::borrow_raw(fd), wr) };
    eprintln!("  ioctl result: {:?}", res);

    match res {
        Ok(_) => Ok(()),
        Err(e) => {
            eprintln!("send_binder_transaction error: {:?}", e);
            Err(Error::Binder(e))
        }
    }
}

fn send_binder_transaction_sync(fd: RawFd, data: &[u8]) -> Result<()> {
    eprintln!("DEBUG: data.len() = {}", data.len());
    eprintln!("DEBUG: data as hex: {:02x?}", data);
    send_binder_transaction(fd, data)
}

fn send_bc_reply(fd: BorrowedFd<'_>, payload: &Payload) {
    let tx_data = TransactionData::new(BinderRef(0), 0, 0);
    let (data, _objects) = tx_data.with_payload(&payload.data).build();

    let mut write_buf = Vec::with_capacity(4 + data.len());
    write_buf.extend_from_slice(&BC_REPLY.to_le_bytes());
    write_buf.extend_from_slice(&data);

    let wr = binder_write_read {
        write_buffer: write_buf.as_ptr() as BinderUintptrT,
        write_size: write_buf.len() as BinderSizeT,
        write_consumed: 0,
        read_size: 0,
        read_consumed: 0,
        read_buffer: 0,
    };

    let res = unsafe { rustix::ioctl::ioctl(fd, wr) };
    if res.is_err() {
        let err = std::io::Error::last_os_error();
        eprintln!("send_bc_reply error: {:?}", err);
    }
}

fn read_binder_reply_sync(fd: RawFd) -> Result<Payload> {
    let read_buf_size = 256 * 1024;
    let mut read_buf = vec![0u8; read_buf_size];

    loop {
        let wr = binder_write_read {
            write_size: 0,
            write_consumed: 0,
            write_buffer: 0,
            read_size: read_buf_size as BinderSizeT,
            read_consumed: 0,
            read_buffer: read_buf.as_mut_ptr() as BinderUintptrT,
        };

        let res = unsafe { rustix::ioctl::ioctl(rustix::fd::BorrowedFd::borrow_raw(fd), wr) };

        match res {
            Ok(_) => {
                let consumed = wr.read_consumed as usize;
                if consumed == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    continue;
                }

                let num_cmds = consumed / 4;
                let ptr = read_buf.as_ptr() as *const u32;

                for i in 0..num_cmds {
                    let cmd = unsafe { *ptr.offset(i as isize) };
                    eprintln!("Child reply: cmd=0x{:x}", cmd);

                    match cmd {
                        BR_REPLY => {
                            eprintln!("Child: got BR_REPLY");
                            let (data, _objects) =
                                TransactionData::parse_reply(&read_buf[..consumed]);
                            return Ok(Payload::with_data(data));
                        }
                        BR_TRANSACTION_COMPLETE => {
                            eprintln!("Child: got BR_TRANSACTION_COMPLETE");
                        }
                        BR_SPAWN_LOOPER => {
                            eprintln!("Child: got BR_SPAWN_LOOPER");
                        }
                    }
                }
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    continue;
                }
                return Err(Error::Binder(e));
            }
        }
    }
}

async fn set_context_manager(fd: BorrowedFd<'_>) -> Result<()> {
    let mut fbo = flat_binder_object::default();
    fbo.hdr.type_ = BINDER_TYPE_BINDER;
    fbo.binder = 0;
    fbo.cookie = 0;

    let fbo_ptr = &fbo as *const flat_binder_object;

    eprintln!(
        "Calling BINDER_SET_CONTEXT_MGR ioctl with fbo at {:p}...",
        fbo_ptr
    );
    let res = unsafe { rustix::ioctl::ioctl(fd, SetContextMGR(fbo)) }.map_err(Error::Binder);

    if let Err(err) = res {
        eprintln!("BINDER_SET_CONTEXT_MGR error: {:?}", err);
        return Err(err);
    }

    eprintln!("Context manager set successfully!");
    Ok(())
}

fn parse_br_transaction(
    _fd: BorrowedFd<'_>,
    wr: &binder_write_read,
    _commands: &[u32],
) -> Option<Transaction> {
    let data_size_offset = 7 * 8;
    let offsets_size_offset = 8 * 8;

    let data = unsafe {
        std::slice::from_raw_parts(wr.read_buffer as *const u8, wr.read_consumed as usize)
    };

    if data.len() < std::mem::size_of::<binder_transaction_data>() {
        return None;
    }

    let data_size = u64::from_le_bytes(
        data[data_size_offset..data_size_offset + 8]
            .try_into()
            .ok()?,
    );
    let _offsets_size = u64::from_le_bytes(
        data[offsets_size_offset..offsets_size_offset + 8]
            .try_into()
            .ok()?,
    );

    let data_start = std::mem::size_of::<binder_transaction_data>();
    let offsets_start = data_start + data_size as usize;

    let payload_data = if data_size > 0 {
        data[data_start..offsets_start].to_vec()
    } else {
        Vec::new()
    };

    let (reply_tx, _reply_rx) = oneshot::channel();

    let tx = Transaction {
        code: u32::from_le_bytes(data[4 * 8..4 * 8 + 4].try_into().ok()?),
        payload: Payload::with_data(payload_data),
        reply_tx,
    };

    Some(tx)
}

fn extract_cookie(wr: &binder_write_read) -> BinderUintptrT {
    let data = unsafe {
        std::slice::from_raw_parts(
            wr.read_buffer as *const u8,
            std::mem::size_of::<binder_transaction_data>(),
        )
    };

    if data.len() >= std::mem::size_of::<binder_transaction_data>() {
        let cookie_offset = 8;
        BinderUintptrT::from_le_bytes(
            data[cookie_offset..cookie_offset + 8]
                .try_into()
                .ok()
                .unwrap_or([0; 8]),
        )
    } else {
        0
    }
}

fn handle_br_reply(pending_replies: &mut VecDeque<PendingReply>, wr: &binder_write_read) {
    if let Some(pending) = pending_replies.pop_front() {
        let reply_data = unsafe {
            std::slice::from_raw_parts(wr.read_buffer as *const u8, wr.read_consumed as usize)
        };

        let (data, _objects) = TransactionData::parse_reply(reply_data);
        let payload = Payload::with_data(data);
        let _ = pending.tx.send(Ok(payload));
    }
}

fn handle_br_dead_reply(pending_replies: &mut VecDeque<PendingReply>) {
    if let Some(pending) = pending_replies.pop_front() {
        let _ = pending.tx.send(Err(Error::DeadReply));
    }
}

fn process_reply_queue(fd: BorrowedFd, reply_queue: &mut VecDeque<Payload>) {
    while let Some(payload) = reply_queue.pop_front() {
        let payload: &Payload = &payload;
        let tx_data = TransactionData::new(BinderRef(0), 0, 0);
        let (data, _objects) = tx_data.with_payload(&payload.data).build();

        let bc_reply_cmd = BC_REPLY;
        let mut write_buf = Vec::with_capacity(4 + data.len());
        write_buf.extend_from_slice(&bc_reply_cmd.to_le_bytes());
        write_buf.extend_from_slice(&data);

        let wr = binder_write_read {
            write_buffer: write_buf.as_ptr() as BinderUintptrT,
            write_size: write_buf.len() as BinderSizeT,
            write_consumed: 0,
            read_size: 0,
            read_consumed: 0,
            read_buffer: 0,
        };

        unsafe {
            let _ = rustix::ioctl::ioctl(fd, wr);
        }
    }
}

fn shutdown_actor(
    pending_replies: &mut VecDeque<PendingReply>,
    registry: &mut HashMap<BinderUintptrT, mpsc::Sender<Transaction>>,
) {
    while let Some(pending) = pending_replies.pop_front() {
        let _ = pending.tx.send(Err(Error::Shutdown));
    }
    registry.clear();
}

pub struct BinderProxy {
    actor: std::sync::Arc<BinderDevice>,
}

impl BinderProxy {
    pub fn new(actor: std::sync::Arc<BinderDevice>) -> Self {
        BinderProxy { actor }
    }

    pub async fn transact(&self, handle: BinderRef, code: u32, data: &[u8]) -> Result<Vec<u8>> {
        let payload = Payload::with_data(data.to_vec());
        self.actor
            .transact(handle, code, payload)
            .await
            .map(|p| p.data)
    }
}

pub struct BinderService<T: Send + Sync + 'static> {
    actor: std::sync::Arc<BinderDevice>,
    handler: std::sync::Arc<T>,
}

impl<T: Send + Sync + 'static> BinderService<T> {
    pub fn new(actor: std::sync::Arc<BinderDevice>, handler: std::sync::Arc<T>) -> Self {
        BinderService { actor, handler }
    }
}

#[tokio::test]
async fn test_device_creation() {
    let file = std::fs::File::open("/dev/binderfs/testbinder").expect(
        "Could not open /dev/binderfs/testbinder. Run: sudo ./target/debug/examples/new_device",
    );
    let device = BinderDevice::open(file)
        .await
        .expect("Could not create device");
    drop(device);
}

#[tokio::test]
async fn test_object_registration() {
    let file = std::fs::File::open("/dev/binderfs/testbinder").expect(
        "Could not open /dev/binderfs/testbinder. Run: sudo ./target/debug/examples/new_device",
    );
    let device = BinderDevice::open(file)
        .await
        .expect("Could not create device");
    let obj = device.register_object();
    assert_eq!(obj.cookie(), 0x12345678);
    drop(obj);
    drop(device);
}
