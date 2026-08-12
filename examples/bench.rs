//! two-process benchmark for binderbinder
//!
//! run with `cargo run --release --example bench`. it forks itself into a service
//! child (context manager) and a client parent, so there's nothing to start by hand —
//! but the binderfs test pool has to exist first:
//!
//!   `sudo cargo run --example setup_test_pool`
//!
//! or point it at a device you already own with `--device /dev/binderfs/testbinder`.
//! with no root at all, binderfs mounts fine in a user namespace:
//!
//!   `unshare -Urm --propagation private sh -c 'mkdir -p /tmp/bfs && mount -t binder binder \
//!     /tmp/bfs && exec ./target/release/examples/bench --device /tmp/bfs/binder'`
//!
//! this is deliberately the same benchmark as `../strong-ipc/examples/bench.rs`: same
//! phases, same table headers, same statistics, same raw-socket floor, so the two
//! outputs can be read side by side. what differs is what a "capability" is — over
//! binder a message can carry binder refs (handles to objects, refcounted by the
//! kernel) *and* file descriptors, so both get their own latency and batching rows.
//!
//! measures, for a real cross-process round trip:
//!   - latency (p50/p90/p99/max) with data only, with a binder ref, and with an fd
//!   - throughput (transactions/s and bytes/s) both blocking-concurrent and one-way
//!   - cpu seconds burned on both sides, and rss/peak-rss on both sides
//!   - file descriptor pressure, sampled continuously on both processes
//!
//! the fd watchdog is the loud one: if either process crosses `FD_WARN_FRACTION` of its
//! RLIMIT_NOFILE the whole run stops immediately with a banner and a nonzero exit. pass
//! `--fd-limit N` to squeeze both processes into a small descriptor budget on purpose.
//!
//! flags: --device PATH, --fd-limit N, --jobs N, --loopers N, --iters N, --secs N,
//!        --churn SECS, --soak SECS, --quick

use std::{
    os::fd::{AsFd, AsRawFd, OwnedFd},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use binderbinder::{
    BinderDevice, TransactionHandler,
    binder_object::BinderObject,
    device::Transaction,
    payload::{BinderObjectType, PayloadBuilder, PayloadReader},
    sys::{
        BinderCommand, BinderObjectHeader, BinderPtrCookie, BinderReturn, BinderTransactionData,
        BinderTransactionDataPtrs, BinderType, BinderWriteRead, FlatBinderFlags, FlatBinderObject,
        FlatBinderObjectData, SetContextMGR, SetMaxThreads, TransactionFlags,
        TransactionTarget as SysTransactionTarget,
    },
    test_pool::{PoolNode, mount_path, node_path, pool_size},
};
use nix::{
    sys::socket::{AddressFamily, MsgFlags, SockFlag, SockType, recv, send, socketpair},
    unistd::{ForkResult, Pid, fork, pipe, read, write},
};

/// stop the run once either side is using this much of its descriptor budget
const FD_WARN_FRACTION: f64 = 0.80;
/// how often the watchdog samples /proc
const FD_SAMPLE_INTERVAL: Duration = Duration::from_millis(5);

const ECHO_CODE: u32 = 1;
const STATS_CODE: u32 = 2;
const RESET_CODE: u32 = 3;

// ---------------------------------------------------------------- /proc helpers

fn clk_tck() -> f64 {
    (unsafe { libc::sysconf(libc::_SC_CLK_TCK) }) as f64
}

/// utime+stime for `pid`, in seconds
fn cpu_seconds(pid: u32) -> Option<f64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm can contain spaces and parens, so split on the *last* ')'
    let rest = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // after the ')' index 0 is `state` (field 3), so utime (14) is 11 and stime (15) is 12
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some((utime + stime) as f64 / clk_tck())
}

fn status_kb(pid: u32, key: &str) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find(|l| l.starts_with(key))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn rss_kb(pid: u32) -> u64 {
    status_kb(pid, "VmRSS:").unwrap_or(0)
}
fn peak_rss_kb(pid: u32) -> u64 {
    status_kb(pid, "VmHWM:").unwrap_or(0)
}

fn fd_count(pid: u32) -> usize {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .map(|d| d.count())
        .unwrap_or(0)
}

fn nofile_limit() -> u64 {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) };
    lim.rlim_cur
}

fn set_nofile_limit(soft: u64) {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) };
    lim.rlim_cur = soft.min(lim.rlim_max);
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) };
}

// ---------------------------------------------------------------- fd watchdog

struct FdWatch {
    self_pid: u32,
    child_pid: u32,
    limit: u64,
    threshold: usize,
    /// high-water marks, so a phase can report the worst it saw
    peak_self: AtomicU64,
    peak_child: AtomicU64,
    tripped: AtomicBool,
    /// when recording, every sample also logs (seconds, client rss, server rss, server fds)
    /// so a long phase can show whether usage plateaus or climbs
    timeline: Mutex<Option<(Instant, Vec<(f64, u64, u64, u64)>)>>,
}

impl FdWatch {
    fn sample(&self) {
        let (mine, theirs) = (fd_count(self.self_pid), fd_count(self.child_pid));
        self.peak_self.fetch_max(mine as u64, Ordering::Relaxed);
        self.peak_child.fetch_max(theirs as u64, Ordering::Relaxed);
        if let Ok(mut guard) = self.timeline.try_lock()
            && let Some((start, log)) = guard.as_mut()
        {
            let t = start.elapsed().as_secs_f64();
            if log.last().is_none_or(|(last, ..)| t - last >= 1.0) {
                log.push((
                    t,
                    rss_kb(self.self_pid),
                    rss_kb(self.child_pid),
                    theirs as u64,
                ));
            }
        }
        if (mine >= self.threshold || theirs >= self.threshold)
            && !self.tripped.swap(true, Ordering::SeqCst)
        {
            self.abort(mine, theirs, "descriptor budget exceeded");
        }
    }

    /// prints the banner, kills the child and leaves. deliberately does not unwind —
    /// running out of fds mid-benchmark makes every number after it a lie
    fn abort(&self, mine: usize, theirs: usize, why: &str) -> ! {
        eprintln!();
        eprintln!("{}", "!".repeat(78));
        eprintln!("!! OUT OF FILE DESCRIPTORS — BENCHMARK STOPPED");
        eprintln!("!! reason: {why}");
        eprintln!("!!");
        eprintln!(
            "!!   client (pid {:>7}): {:>7} fds open",
            self.self_pid, mine
        );
        eprintln!(
            "!!   server (pid {:>7}): {:>7} fds open",
            self.child_pid, theirs
        );
        eprintln!(
            "!!   RLIMIT_NOFILE (soft): {:>7}   stop threshold: {} ({:.0}%)",
            self.limit,
            self.threshold,
            FD_WARN_FRACTION * 100.0
        );
        eprintln!("!!");
        eprintln!("!! results printed before this point are still valid; everything the");
        eprintln!("!! run would have measured after it is discarded.");
        eprintln!("{}", "!".repeat(78));
        let _ = nix::sys::signal::kill(
            Pid::from_raw(self.child_pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
        std::process::exit(3);
    }

    fn reset_peaks(&self) {
        self.peak_self.store(0, Ordering::Relaxed);
        self.peak_child.store(0, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------- service side

/// echoes whatever it gets back to the caller — bytes, binder refs and fds alike
///
/// echoing the objects rather than dropping them is the whole point: it makes the
/// kernel translate a handle/descriptor in *both* directions per transaction, so the
/// capability rows measure a full there-and-back translation, not half of one.
#[derive(Debug, Default)]
struct EchoService {
    /// one-way transactions actually handled, for the fire-and-forget throughput phase
    one_way: AtomicU64,
    one_way_bytes: AtomicU64,
}

impl TransactionHandler for EchoService {
    async fn handle(self: Arc<Self>, mut transaction: Transaction) -> PayloadBuilder<'static> {
        let mut builder = PayloadBuilder::new();
        match transaction.code {
            STATS_CODE => {
                builder.push_bytes(&self.one_way.load(Ordering::Relaxed).to_ne_bytes());
                builder.push_bytes(&self.one_way_bytes.load(Ordering::Relaxed).to_ne_bytes());
                return builder;
            }
            RESET_CODE => {
                self.one_way.store(0, Ordering::Relaxed);
                self.one_way_bytes.store(0, Ordering::Relaxed);
                builder.push_bytes(b"ok");
                return builder;
            }
            _ => {}
        }
        echo_into(&mut transaction.payload, &mut builder);
        builder
    }

    async fn handle_one_way(self: Arc<Self>, transaction: Transaction) {
        self.one_way.fetch_add(1, Ordering::Relaxed);
        self.one_way_bytes.fetch_add(
            transaction.payload.bytes_until_next_obj() as u64,
            Ordering::Relaxed,
        );
        // drop the payload immediately — holding it stalls the sender's async buffer
        drop(transaction.payload);
    }
}

/// copies the data block, then every ref and fd, out of `reader` and into `builder`
///
/// the shape is fixed by `Client::message`: `[u32 refs][u32 fds][payload]`, then that
/// many binder refs, then that many fds. scanning for objects instead — the way
/// `examples/self_transaction.rs` does — can't be used here, because
/// `bytes_until_next_obj` looks for an offset *strictly* past the cursor and so misses
/// an object that starts exactly where the cursor already is. two adjacent objects (any
/// row with more than one ref or fd per transaction) then get read back as raw bytes.
fn echo_into(reader: &mut PayloadReader, builder: &mut PayloadBuilder<'static>) {
    // reliable at the cursor's start position: the first object offset is always past 0
    let block = reader.bytes_until_next_obj();
    let Ok(data) = reader.read_bytes(block) else {
        return;
    };
    let (refs, fds) = header_counts(data);
    builder.push_bytes(data);
    for _ in 0..refs {
        let Ok(r) = reader.read_binder_ref() else {
            return;
        };
        builder.push_binder_ref(&r);
    }
    for _ in 0..fds {
        let Ok((fd, cookie)) = reader.read_fd() else {
            return;
        };
        builder.push_owned_fd(fd, cookie);
    }
}

/// `[u32 refs][u32 fds]` off the front of a bench payload
fn header_counts(data: &[u8]) -> (u32, u32) {
    if data.len() < 8 {
        return (0, 0);
    }
    (
        u32::from_ne_bytes(data[..4].try_into().unwrap()),
        u32::from_ne_bytes(data[4..8].try_into().unwrap()),
    )
}

// ------------------------------------------------- raw binder, no binderbinder
//
// this is binderbinder's actual floor: the same driver, the same two processes, the
// same ioctls, with nothing of the library in between — no tokio, no looper pool, no
// object table, no PayloadBuilder/PayloadReader. one thread on each side, hand-rolled
// BC_TRANSACTION / BR_TRANSACTION / BC_REPLY / BR_REPLY.
//
// it needs its own device node, because a binder context has exactly one context
// manager and the library service has already claimed the main one.

/// a device opened by hand: fd plus the mapping the driver hands transaction buffers out
/// of. the mapping is only read by us; the kernel writes into it
struct RawBinder {
    fd: OwnedFd,
    map: *mut std::ffi::c_void,
    map_len: usize,
}

impl Drop for RawBinder {
    fn drop(&mut self) {
        unsafe {
            let _ = rustix::mm::munmap(self.map, self.map_len);
        }
    }
}

impl RawBinder {
    fn open(path: &PathBuf) -> rustix::io::Result<Self> {
        let fd = rustix::fs::open(
            path,
            rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::RDWR,
            rustix::fs::Mode::empty(),
        )?;
        let map_len = 1024 * 1024;
        let map = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                map_len,
                rustix::mm::ProtFlags::READ,
                rustix::mm::MapFlags::PRIVATE | rustix::mm::MapFlags::NORESERVE,
                &fd,
                0,
            )?
        };
        unsafe { rustix::ioctl::ioctl(fd.as_fd(), SetMaxThreads(1))? };
        Ok(Self { fd, map, map_len })
    }

    /// one BINDER_WRITE_READ: hand the driver `write`, take back whatever it has
    ///
    /// returns how much of `write` the driver consumed and how many bytes it wrote into
    /// `read`, which is all the caller needs to drive the command stream itself
    fn write_read(&self, write: &[u8], read: &mut [u8]) -> rustix::io::Result<(usize, usize)> {
        let mut bwr = BinderWriteRead {
            write_size: write.len(),
            write_consumed: 0,
            write_buffer: write.as_ptr() as usize,
            read_size: read.len(),
            read_consumed: 0,
            read_buffer: read.as_mut_ptr() as usize,
        };
        rustix::io::retry_on_intr(|| unsafe { rustix::ioctl::ioctl(self.fd.as_fd(), &mut bwr) })?;
        Ok((bwr.write_consumed, bwr.read_consumed))
    }
}

fn push_cmd(buf: &mut Vec<u8>, cmd: BinderCommand) {
    buf.extend_from_slice(&cmd.as_u32().to_ne_bytes());
}

fn push_struct<T>(buf: &mut Vec<u8>, value: &T) {
    let bytes =
        unsafe { std::slice::from_raw_parts(value as *const T as *const u8, size_of::<T>()) };
    buf.extend_from_slice(bytes);
}

/// how many payload bytes follow each BR_ code in the read stream
fn br_payload_len(br: BinderReturn) -> Option<usize> {
    Some(match br {
        BinderReturn::NOOP
        | BinderReturn::TRANSACTION_COMPLETE
        | BinderReturn::SPAWN_LOOPER
        | BinderReturn::DEAD_REPLY
        | BinderReturn::FAILED_REPLY
        | BinderReturn::FROZEN_REPLY
        | BinderReturn::ONEWAY_SPAM_SUSPECT
        | BinderReturn::OK
        | BinderReturn::FINISHED => 0,
        BinderReturn::ERROR => size_of::<i32>(),
        BinderReturn::TRANSACTION | BinderReturn::REPLY => size_of::<BinderTransactionData>(),
        BinderReturn::ACQUIRE
        | BinderReturn::RELEASE
        | BinderReturn::INCREFS
        | BinderReturn::DECREFS => size_of::<BinderPtrCookie>(),
        BinderReturn::DEAD_BINDER
        | BinderReturn::CLEAR_DEATH_NOTIFICATION_DONE
        | BinderReturn::CLEAR_FREEZE_NOTIFICATION_DONE => size_of::<usize>(),
        // anything else carries a payload we don't know how to skip, so the stream
        // can't be parsed past it
        _ => return None,
    })
}

/// raw echo service: becomes context manager on its own node and replies to every
/// transaction with the exact buffer the driver handed it
fn raw_binder_service(path: PathBuf) {
    let dev = match RawBinder::open(&path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("raw binder service: could not open {}: {e}", path.display());
            return;
        }
    };
    let flat = FlatBinderObject {
        hdr: BinderObjectHeader {
            type_: BinderType::BINDER,
        },
        flags: FlatBinderFlags::ACCEPTS_FDS,
        data: FlatBinderObjectData { binder: 1 },
        cookie: 0,
    };
    if unsafe { rustix::ioctl::ioctl(dev.fd.as_fd(), SetContextMGR(flat)) }.is_err() {
        eprintln!("raw binder service: could not become context manager on this node");
        return;
    }

    let mut write = Vec::with_capacity(256);
    push_cmd(&mut write, BinderCommand::ENTER_LOOPER);
    let mut read = vec![0u8; 4096];
    loop {
        let Ok((_, got)) = dev.write_read(&write, &mut read) else {
            return;
        };
        write.clear();
        let mut at = 0;
        while at + 4 <= got {
            let br =
                BinderReturn::from_u32(u32::from_ne_bytes(read[at..at + 4].try_into().unwrap()));
            at += 4;
            let Some(len) = br_payload_len(br) else {
                return;
            };
            if br == BinderReturn::TRANSACTION {
                let tr = unsafe {
                    std::ptr::read_unaligned(read[at..].as_ptr() as *const BinderTransactionData)
                };
                // the reply points straight at the buffer the driver just gave us: the
                // kernel copies out of our address space, so echoing costs no copy here
                let mut reply = tr;
                reply.target = SysTransactionTarget { handle: 0 };
                reply.cookie = 0;
                reply.flags = TransactionFlags::empty();
                reply.offsets_size = 0;
                push_cmd(&mut write, BinderCommand::REPLY);
                push_struct(&mut write, &reply);
                push_cmd(&mut write, BinderCommand::FREE_BUFFER);
                write.extend_from_slice(&tr.data.buffer.to_ne_bytes());
            }
            at += len;
        }
    }
}

/// raw client: one BC_TRANSACTION, then pump the command stream until BR_REPLY
struct RawBinderClient {
    dev: RawBinder,
    read: Vec<u8>,
}

impl RawBinderClient {
    fn open(path: &PathBuf) -> rustix::io::Result<Self> {
        Ok(Self {
            dev: RawBinder::open(path)?,
            read: vec![0u8; 4096],
        })
    }

    /// returns the reply's payload size, or None if the driver reported a failure
    fn round_trip(&mut self, data: &[u8]) -> Option<usize> {
        let tr = BinderTransactionData {
            target: SysTransactionTarget { handle: 0 },
            cookie: 0,
            code: ECHO_CODE,
            flags: TransactionFlags::empty(),
            sender_pid: 0,
            sender_euid: 0,
            data_size: data.len(),
            offsets_size: 0,
            data: BinderTransactionDataPtrs {
                buffer: data.as_ptr() as usize,
                offsets: 0,
            },
        };
        let mut write = Vec::with_capacity(128);
        push_cmd(&mut write, BinderCommand::TRANSACTION);
        push_struct(&mut write, &tr);

        loop {
            let (_, got) = self.dev.write_read(&write, &mut self.read).ok()?;
            write.clear();
            let mut at = 0;
            while at + 4 <= got {
                let br = BinderReturn::from_u32(u32::from_ne_bytes(
                    self.read[at..at + 4].try_into().unwrap(),
                ));
                at += 4;
                let len = br_payload_len(br)?;
                match br {
                    BinderReturn::REPLY => {
                        let reply = unsafe {
                            std::ptr::read_unaligned(
                                self.read[at..].as_ptr() as *const BinderTransactionData
                            )
                        };
                        let size = reply.data_size;
                        // freeing is part of the round trip's work, but it goes out with
                        // the next transaction's write rather than costing its own ioctl
                        let mut free = Vec::with_capacity(16);
                        push_cmd(&mut free, BinderCommand::FREE_BUFFER);
                        free.extend_from_slice(&reply.data.buffer.to_ne_bytes());
                        let _ = self.dev.write_read(&free, &mut []);
                        return Some(size);
                    }
                    BinderReturn::DEAD_REPLY | BinderReturn::FAILED_REPLY => return None,
                    BinderReturn::ERROR => return None,
                    _ => {}
                }
                at += len;
            }
        }
    }
}

/// bare seqpacket echo on the pre-forked socketpair — no binder, no library, no
/// runtime. a cross-transport reference point, *not* binderbinder's floor: it measures
/// a different kernel primitive entirely
fn raw_echo(sock: OwnedFd) {
    let fd = sock.as_raw_fd();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        match recv(fd, &mut buf, MsgFlags::empty()) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if send(fd, &buf[..n], MsgFlags::empty()).is_err() {
                    return;
                }
            }
        }
    }
}

// ---------------------------------------------------------------- statistics

struct Stats {
    p50: f64,
    p90: f64,
    p99: f64,
    p999: f64,
    max: f64,
    mean: f64,
    /// every sample, sorted, in ns — kept so a phase can also be drawn as a histogram.
    /// percentiles hide the shape, and the shape is where scheduler tails and
    /// per-object costs actually show up
    samples: Vec<u64>,
}

fn summarize(mut samples: Vec<u64>) -> Stats {
    samples.sort_unstable();
    let at = |q: f64| -> f64 {
        let i = ((samples.len() as f64 - 1.0) * q).round() as usize;
        samples[i] as f64 / 1000.0
    };
    let mean = samples.iter().sum::<u64>() as f64 / samples.len() as f64 / 1000.0;
    Stats {
        p50: at(0.50),
        p90: at(0.90),
        p99: at(0.99),
        p999: at(0.999),
        max: *samples.last().unwrap() as f64 / 1000.0,
        mean,
        samples,
    }
}

/// √2-spaced buckets from the fastest sample up, drawn as bars
///
/// log spacing keeps a long tail readable next to a tight mode without the tail
/// squashing everything else into one row
fn histogram(title: &str, samples: &[u64]) {
    if samples.is_empty() {
        return;
    }
    let lo = (samples[0] as f64 / 1000.0).max(0.1);
    let hi = (*samples.last().unwrap() as f64 / 1000.0).max(lo * 1.001);
    let step = std::f64::consts::SQRT_2;
    let buckets = ((hi / lo).log(step).ceil() as usize + 1).clamp(1, 24);
    let mut counts = vec![0usize; buckets];
    for s in samples {
        let us = *s as f64 / 1000.0;
        let i = ((us / lo).log(step).floor() as isize).clamp(0, buckets as isize - 1) as usize;
        counts[i] += 1;
    }
    let peak = counts.iter().copied().max().unwrap_or(1).max(1);
    let total = samples.len() as f64;
    println!("  {title}  (n = {})", samples.len());
    for (i, count) in counts.iter().enumerate() {
        let edge = lo * step.powi(i as i32);
        let next = edge * step;
        // sub-1% buckets still get a mark, so the tail stays visible
        let bar = if *count == 0 {
            0
        } else {
            ((*count * 44) / peak).max(1)
        };
        println!(
            "   {edge:>7.2}–{next:>7.2} µs │{:<44}│ {count:>7}  {:>5.1}%",
            "█".repeat(bar),
            *count as f64 / total * 100.0
        );
    }
}

/// a phase's cost on both processes, so cpu can be reported per transaction
struct Usage {
    client_cpu: f64,
    server_cpu: f64,
    wall: f64,
}

struct Meter {
    self_pid: u32,
    child_pid: u32,
    t0: Instant,
    c0: f64,
    s0: f64,
}

impl Meter {
    fn start(self_pid: u32, child_pid: u32) -> Self {
        Self {
            self_pid,
            child_pid,
            t0: Instant::now(),
            c0: cpu_seconds(self_pid).unwrap_or(0.0),
            s0: cpu_seconds(child_pid).unwrap_or(0.0),
        }
    }
    fn stop(self) -> Usage {
        Usage {
            client_cpu: cpu_seconds(self.self_pid).unwrap_or(0.0) - self.c0,
            server_cpu: cpu_seconds(self.child_pid).unwrap_or(0.0) - self.s0,
            wall: self.t0.elapsed().as_secs_f64(),
        }
    }
}

// ---------------------------------------------------------------- client side

struct Client {
    device: Arc<BinderDevice>,
    /// a locally registered object; a ref to it is the capability we hand out, exactly
    /// as a service would hand a client a callback interface
    reply_obj: BinderObject<EchoService>,
    /// something cheap to pass around as a descriptor
    devnull: OwnedFd,
    watch: Arc<FdWatch>,
}

// safe for the same reason the device is: everything inside is Send+Sync, and the
// threads below only ever call &self methods
unsafe impl Send for Client {}
unsafe impl Sync for Client {}

impl Client {
    /// `[u32 refs][u32 fds][payload]`, then the refs, then the fds — see `echo_into`
    fn message(&self, payload: &[u8], refs: usize, fds: usize) -> PayloadBuilder<'_> {
        let mut b = PayloadBuilder::new();
        b.push_bytes(&(refs as u32).to_ne_bytes());
        b.push_bytes(&(fds as u32).to_ne_bytes());
        b.push_bytes(payload);
        for _ in 0..refs {
            b.push_binder_ref(&self.reply_obj);
        }
        for _ in 0..fds {
            b.push_fd(self.devnull.as_fd(), 0);
        }
        b
    }

    /// one blocking transaction to the service, reply buffer included
    fn round_trip(
        &self,
        payload: &[u8],
        refs: usize,
        fds: usize,
    ) -> binderbinder::Result<PayloadReader> {
        self.device
            .transact_blocking(
                self.device.context_manager(),
                ECHO_CODE,
                self.message(payload, refs, fds),
            )
            .map(|(_, reply)| reply)
    }

    fn one_way(&self, payload: &[u8], refs: usize) -> binderbinder::Result<()> {
        self.device.transact_one_way(
            self.device.context_manager(),
            ECHO_CODE,
            self.message(payload, refs, 0),
        )
    }

    /// asks the service how many one-way transactions it has actually handled
    fn service_stats(&self) -> (u64, u64) {
        let mut reply = self
            .device
            .transact_blocking(
                self.device.context_manager(),
                STATS_CODE,
                PayloadBuilder::new(),
            )
            .expect("stats transaction failed")
            .1;
        let bytes = reply.read_bytes(16).expect("short stats reply").to_vec();
        (
            u64::from_ne_bytes(bytes[..8].try_into().unwrap()),
            u64::from_ne_bytes(bytes[8..].try_into().unwrap()),
        )
    }

    fn reset_stats(&self) {
        let _ = self.device.transact_blocking(
            self.device.context_manager(),
            RESET_CODE,
            PayloadBuilder::new(),
        );
    }

    /// sequential ping-pong: one transaction in flight, so each sample is a full round
    /// trip. the reply buffer is freed *after* the clock stops, matching what
    /// strong-ipc's bench counts (delivery, not teardown)
    fn latency(&self, payload: usize, refs: usize, fds: usize, iters: usize) -> (Stats, Usage) {
        let data = vec![0x41u8; payload];
        // warm up so page faults, looper spawns and lazy allocations stay out of the samples
        for _ in 0..1000usize.min(iters) {
            drop(self.round_trip(&data, refs, fds));
        }
        self.watch.reset_peaks();

        let mut samples = Vec::with_capacity(iters);
        let meter = Meter::start(self.watch.self_pid, self.watch.child_pid);
        let mut last_sample = Instant::now();
        for _ in 0..iters {
            let t0 = Instant::now();
            let reply = self.round_trip(&data, refs, fds);
            let elapsed = t0.elapsed();
            match reply {
                Ok(reply) => {
                    samples.push(elapsed.as_nanos() as u64);
                    // dropping frees the kernel buffer, releases the echoed refs and
                    // closes the echoed descriptors
                    drop(reply);
                }
                Err(e) => {
                    eprintln!(
                        "  transaction failed after {} samples: {e:?}",
                        samples.len()
                    );
                    break;
                }
            }
            if last_sample.elapsed() > FD_SAMPLE_INTERVAL {
                self.watch.sample();
                last_sample = Instant::now();
            }
        }
        (summarize(samples), meter.stop())
    }

    /// blocking transactions can't be pipelined on one thread, so saturation comes from
    /// `jobs` threads each holding one transaction in flight
    fn throughput_blocking(
        &self,
        payload: usize,
        refs: usize,
        fds: usize,
        jobs: usize,
        duration: Duration,
    ) -> (u64, u64, Usage) {
        let data = vec![0x41u8; payload];
        drop(self.round_trip(&data, refs, fds));
        self.watch.reset_peaks();

        let done = AtomicU64::new(0);
        let failed = AtomicU64::new(0);
        let meter = Meter::start(self.watch.self_pid, self.watch.child_pid);
        let deadline = Instant::now() + duration;
        std::thread::scope(|scope| {
            for job in 0..jobs {
                let (data, done, failed) = (&data, &done, &failed);
                let handle = tokio::runtime::Handle::current();
                scope.spawn(move || {
                    let _guard = handle.enter();
                    let mut n = 0u64;
                    let mut last_sample = Instant::now();
                    loop {
                        match self.round_trip(data, refs, fds) {
                            Ok(reply) => drop(reply),
                            Err(_) => {
                                failed.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        n += 1;
                        if n.is_multiple_of(16) {
                            if Instant::now() >= deadline {
                                break;
                            }
                            // one sampler is enough; the others would just fight over /proc
                            if job == 0 && last_sample.elapsed() > FD_SAMPLE_INTERVAL {
                                self.watch.sample();
                                last_sample = Instant::now();
                            }
                        }
                    }
                    done.fetch_add(n, Ordering::Relaxed);
                });
            }
        });
        let usage = meter.stop();
        (
            done.load(Ordering::Relaxed),
            failed.load(Ordering::Relaxed),
            usage,
        )
    }

    /// fire-and-forget: no reply, so a single thread can keep pushing. the service's own
    /// counter says how many actually landed, which is the only honest way to tell a
    /// fast one-way path from a lossy one
    fn throughput_one_way(
        &self,
        payload: usize,
        refs: usize,
        duration: Duration,
    ) -> (u64, u64, u64, Usage) {
        let data = vec![0x41u8; payload];
        self.reset_stats();
        self.watch.reset_peaks();

        let mut sent = 0u64;
        let mut rejected = 0u64;
        let meter = Meter::start(self.watch.self_pid, self.watch.child_pid);
        let deadline = Instant::now() + duration;
        let mut last_sample = Instant::now();
        loop {
            match self.one_way(&data, refs) {
                Ok(()) => sent += 1,
                // the async buffer is half the mapping; a full one is backpressure,
                // reported rather than hidden
                Err(_) => {
                    rejected += 1;
                    std::thread::yield_now();
                }
            }
            if (sent + rejected).is_multiple_of(64) {
                if Instant::now() >= deadline {
                    break;
                }
                if last_sample.elapsed() > FD_SAMPLE_INTERVAL {
                    self.watch.sample();
                    last_sample = Instant::now();
                }
            }
        }
        let usage = meter.stop();
        // let the tail land before asking for the count
        let drain_until = Instant::now() + Duration::from_secs(2);
        let mut handled = self.service_stats().0;
        while handled < sent && Instant::now() < drain_until {
            std::thread::sleep(Duration::from_millis(20));
            let next = self.service_stats().0;
            if next == handled {
                break;
            }
            handled = next;
        }
        (sent, handled, rejected, usage)
    }
}

// ---------------------------------------------------------------- reporting

fn header(title: &str) {
    println!();
    println!(
        "── {title} {}",
        "─".repeat(72usize.saturating_sub(title.len()))
    );
}

fn fmt_bytes_per_sec(b: f64) -> String {
    const UNITS: [&str; 4] = ["B/s", "KiB/s", "MiB/s", "GiB/s"];
    let mut v = b;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}

// ---------------------------------------------------------------- args

struct Args {
    device: Option<PathBuf>,
    /// a second node, for the raw-binder floor's own context manager
    raw_device: Option<PathBuf>,
    fd_limit: Option<u64>,
    jobs: usize,
    loopers: usize,
    iters: usize,
    secs: u64,
    churn: u64,
    soak: Option<u64>,
}

fn parse_args() -> Args {
    let argv: Vec<String> = std::env::args().collect();
    let val = |name: &str| -> Option<String> {
        argv.iter()
            .position(|a| a == name)
            .and_then(|i| argv.get(i + 1))
            .cloned()
    };
    let num = |name: &str, default: u64| -> u64 {
        val(name).and_then(|s| s.parse().ok()).unwrap_or(default)
    };
    let quick = argv.iter().any(|a| a == "--quick");
    Args {
        device: val("--device").map(PathBuf::from),
        raw_device: val("--raw-device").map(PathBuf::from),
        fd_limit: val("--fd-limit").and_then(|s| s.parse().ok()),
        jobs: num("--jobs", 4) as usize,
        loopers: num("--loopers", 5) as usize,
        iters: num("--iters", if quick { 2_000 } else { 20_000 }) as usize,
        secs: num("--secs", if quick { 1 } else { 3 }),
        churn: num("--churn", if quick { 5 } else { 30 }),
        soak: val("--soak").and_then(|s| s.parse().ok()),
    }
}

fn open_device(path: &PathBuf, loopers: usize) -> Arc<BinderDevice> {
    let fd = rustix::fs::open(
        path,
        rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::RDWR,
        rustix::fs::Mode::empty(),
    )
    .unwrap_or_else(|e| panic!("could not open binder device {}: {e}", path.display()));
    BinderDevice::from_fd(fd, loopers)
}

// ---------------------------------------------------------------- entry point

fn main() {
    let args = parse_args();

    if let Some(n) = args.fd_limit {
        set_nofile_limit(n);
    }

    // a pool node is claimed for the whole run (and released when this guard drops), so
    // a concurrent test suite can't take the same device out from under us. the raw
    // binder floor needs a *second* node: a binder context has one context manager, and
    // the library service claims the first node's
    let (path, raw_path, _pool_guard) = match &args.device {
        Some(p) => (p.clone(), args.raw_device.clone(), None),
        None => {
            let mount = mount_path();
            if !mount.exists() {
                eprintln!(
                    "no binderfs test pool at {} — run `sudo cargo run --example setup_test_pool`,",
                    mount.display()
                );
                eprintln!("or point this at a device you already have with --device PATH");
                std::process::exit(2);
            }
            let node = PoolNode::acquire();
            let raw = args.raw_device.clone().or_else(|| {
                (0..pool_size())
                    .map(node_path)
                    .find(|p| *p != node.path && p.exists())
            });
            (node.path.clone(), raw, Some(node))
        }
    };

    // both created before the fork so each process inherits one end
    let (raw_client, raw_service) = socketpair(
        AddressFamily::Unix,
        SockType::SeqPacket,
        None,
        SockFlag::empty(),
    )
    .expect("socketpair");
    let (ready_r, ready_w) = pipe().expect("pipe (ready)");

    // fork *before* any tokio runtime exists — forking a running multi-threaded runtime
    // is unsound, and each side needs its own binder_proc in the kernel anyway
    match unsafe { fork() }.expect("fork failed") {
        ForkResult::Child => {
            drop(ready_r);
            drop(raw_client);
            service_main(path, raw_path, args.loopers, raw_service, ready_w);
        }
        ForkResult::Parent { child } => {
            drop(ready_w);
            drop(raw_service);
            let mut buf = [0u8; 1];
            read(&ready_r, &mut buf).expect("read ready signal");
            assert_eq!(buf[0], 1, "service role failed before it became ready");
            drop(ready_r);
            client_main(args, path, raw_path, child, raw_client);
        }
    }
}

/// the service child: becomes context manager, then parks until it's killed
fn service_main(
    path: PathBuf,
    raw_path: Option<PathBuf>,
    loopers: usize,
    raw: OwnedFd,
    ready_w: OwnedFd,
) -> ! {
    // this process parks forever by design, so make sure a client that dies (or gets
    // killed by a `timeout`) can't leave a registered context manager behind
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };

    let rt = tokio::runtime::Runtime::new().expect("build service runtime");
    let _obj = rt.block_on(async {
        let device = open_device(&path, loopers);
        let obj = device.register_object(EchoService::default());
        device
            .set_context_manager(&obj)
            .await
            .expect("set_context_manager (service role)");
        obj
    });
    // both floors live on their own threads so neither can interact with the library's
    // looper pool or its runtime
    std::thread::spawn(move || raw_echo(raw));
    if let Some(raw_path) = raw_path {
        std::thread::spawn(move || raw_binder_service(raw_path));
        // give it a moment to claim its node's context manager before the client
        // starts probing for it
        std::thread::sleep(Duration::from_millis(100));
    }

    write(&ready_w, &[1]).expect("signal ready");
    drop(ready_w);

    // the object above stays registered for as long as we're parked here; the client
    // kills us when it's done
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

fn client_main(args: Args, path: PathBuf, raw_path: Option<PathBuf>, child: Pid, raw: OwnedFd) {
    let self_pid = std::process::id();
    let child_pid = child.as_raw() as u32;

    let limit = nofile_limit();
    let watch = Arc::new(FdWatch {
        self_pid,
        child_pid,
        limit,
        threshold: (limit as f64 * FD_WARN_FRACTION) as usize,
        peak_self: AtomicU64::new(0),
        peak_child: AtomicU64::new(0),
        tripped: AtomicBool::new(false),
        timeline: Mutex::new(None),
    });

    println!("binderbinder two-process benchmark");
    println!("  client pid          {self_pid}");
    println!("  service pid         {child_pid}");
    println!("  device              {}", path.display());
    println!(
        "  raw binder node     {}",
        match &raw_path {
            Some(p) => p.display().to_string(),
            None => "none — raw binder floor skipped (pass --raw-device PATH)".to_string(),
        }
    );
    println!(
        "  loopers per side    {}   client jobs {}",
        args.loopers, args.jobs
    );
    println!(
        "  RLIMIT_NOFILE       {limit} (soft)   stop threshold {} ({:.0}%)",
        watch.threshold,
        FD_WARN_FRACTION * 100.0
    );
    println!(
        "  build               {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );

    // idle footprint, before either side has done any work
    std::thread::sleep(Duration::from_millis(200));
    let idle_client_rss = rss_kb(self_pid);
    let idle_service_rss = rss_kb(child_pid);
    let idle_client_fds = fd_count(self_pid);
    let idle_service_fds = fd_count(child_pid);

    let rt = tokio::runtime::Runtime::new().expect("build client runtime");
    // everything below runs on this thread inside the runtime's context: the blocking
    // transaction API needs a runtime handle, and the device's looper threads need the
    // worker threads left free to run handler tasks
    let _guard = rt.enter();

    let device = open_device(&path, args.loopers);
    let reply_obj = device.register_object(EchoService::default());
    let devnull = rustix::fs::open(
        "/dev/null",
        rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::RDWR,
        rustix::fs::Mode::empty(),
    )
    .expect("open /dev/null");

    // shared so the payload-ceiling probe can hand a transaction to a throwaway thread
    // and walk away from it if it never comes back
    let client = Arc::new(Client {
        device,
        reply_obj,
        devnull,
        watch: watch.clone(),
    });

    header("idle footprint");
    println!("  client   rss {idle_client_rss:>7} KiB   fds {idle_client_fds:>4}");
    println!("  service  rss {idle_service_rss:>7} KiB   fds {idle_service_fds:>4}");

    // ---- self check: a transaction carrying one of everything must come back intact
    {
        let payload = b"binderbinder bench self-check".to_vec();
        let mut reply = client
            .round_trip(&payload, 2, 2)
            .expect("self-check transaction failed");
        let block = reply.bytes_until_next_obj();
        let echoed = reply
            .read_bytes(block)
            .expect("self-check reply had no bytes")
            .to_vec();
        assert_eq!(
            header_counts(&echoed),
            (2, 2),
            "the reply's header did not survive"
        );
        assert_eq!(
            &echoed[8..8 + payload.len()],
            &payload[..],
            "payload did not round-trip intact"
        );
        // every object has to come back, and come back as the right kind: two refs to
        // our own object (so the kernel hands them back as BINDER, not HANDLE) and two
        // descriptors
        let mut refs = 0;
        let mut fds = 0;
        while let Some(kind) = reply.next_object_type() {
            match kind {
                BinderObjectType::BinderObject | BinderObjectType::BinderRef => {
                    reply.read_binder_ref().expect("reading echoed ref");
                    refs += 1;
                }
                BinderObjectType::Fd => {
                    reply.read_fd().expect("reading echoed fd");
                    fds += 1;
                }
                _ => panic!("unexpected object kind in the reply"),
            }
        }
        assert_eq!((refs, fds), (2, 2), "objects did not round-trip");
        drop(reply);
        println!("  self-check: payload, two binder refs and two fds echoed back intact");
    }

    // ---- soak: does anything grow without bound?
    if let Some(secs) = args.soak {
        header(&format!("soak — {secs} s of binder-ref passing"));
        println!(
            "  {:>6}  {:>12} {:>12} {:>10}",
            "t (s)", "client rss", "service rss", "svc fds"
        );
        *watch.timeline.lock().unwrap() = Some((Instant::now(), Vec::new()));
        let (done, failed, u) =
            client.throughput_blocking(64, 1, 0, args.jobs, Duration::from_secs(secs));
        let log = watch.timeline.lock().unwrap().take().unwrap().1;
        for (t, c, s, f) in log.iter().step_by(10) {
            println!("  {t:>6.0}  {c:>8} KiB {s:>8} KiB {f:>10}");
        }
        println!(
            "  {done} transactions ({failed} failed) at {:.0} /s",
            done as f64 / u.wall
        );
        std::thread::sleep(Duration::from_secs(1));
        println!(
            "  at rest: client {} KiB / {} fds, service {} KiB / {} fds",
            rss_kb(self_pid),
            fd_count(self_pid),
            rss_kb(child_pid),
            fd_count(child_pid)
        );
        shutdown(child, path);
        return;
    }

    let mut binder_p50 = std::collections::BTreeMap::new();
    // sample sets worth drawing in full, collected as their phases run
    let mut dists: Vec<(String, Vec<u64>)> = Vec::new();

    // ---- the real floor: the same driver, by hand, with no binderbinder in the way
    header("floor — raw binder ioctl round trip, no binderbinder (µs)");
    match raw_path.as_ref().map(RawBinderClient::open) {
        Some(Ok(mut raw_binder)) => {
            println!(
                "  {:>8}  {:>8} {:>8} {:>8} {:>8} {:>8} {:>9}",
                "payload", "p50", "p90", "p99", "p99.9", "max", "mean"
            );
            for payload in [8usize, 1024, 8192] {
                let data = vec![0x41u8; payload];
                let mut ok = true;
                for _ in 0..1000 {
                    ok &= raw_binder.round_trip(&data).is_some();
                }
                if !ok {
                    println!("  {payload:>8}  → the raw service never replied; skipping");
                    continue;
                }
                let mut samples = Vec::with_capacity(args.iters);
                for _ in 0..args.iters {
                    let t0 = Instant::now();
                    if raw_binder.round_trip(&data).is_none() {
                        break;
                    }
                    samples.push(t0.elapsed().as_nanos() as u64);
                }
                let s = summarize(samples);
                binder_p50.insert(payload, s.p50);
                println!(
                    "  {:>8}  {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>9.2}",
                    payload, s.p50, s.p90, s.p99, s.p999, s.max, s.mean
                );
                if payload == 8 {
                    dists.push(("raw binder ioctl, 8 B".to_string(), s.samples));
                }
            }
            println!("  one thread per side, hand-rolled BC_TRANSACTION/BC_REPLY, no tokio, no");
            println!("  looper pool, no object table. every µs between this and the rows below");
            println!("  is binderbinder's own overhead.");
        }
        Some(Err(e)) => println!("  skipped — could not open the raw binder node: {e}"),
        None => println!("  skipped — no second binder node (pass --raw-device PATH)"),
    }

    // ---- a different transport entirely, for cross-project comparison
    header("reference — raw SOCK_SEQPACKET round trip (different primitive) (µs)");
    println!("  not binderbinder's floor: a unix socket is not the binder driver. it is here");
    println!("  because strong-ipc's bench measures the same thing, so it is the one number");
    println!("  both projects share.");
    println!(
        "  {:>8}  {:>8} {:>8} {:>8} {:>8} {:>8} {:>9}",
        "payload", "p50", "p90", "p99", "p99.9", "max", "mean"
    );
    let mut seqpacket_p50 = std::collections::BTreeMap::new();
    {
        let fd = raw.as_raw_fd();
        let mut rbuf = vec![0u8; 65536];
        for payload in [8usize, 1024, 8192] {
            let data = vec![0x41u8; payload];
            for _ in 0..1000 {
                send(fd, &data, MsgFlags::empty()).unwrap();
                recv(fd, &mut rbuf, MsgFlags::empty()).unwrap();
            }
            let iters = args.iters;
            let mut samples = Vec::with_capacity(iters);
            for _ in 0..iters {
                let t0 = Instant::now();
                send(fd, &data, MsgFlags::empty()).unwrap();
                recv(fd, &mut rbuf, MsgFlags::empty()).unwrap();
                samples.push(t0.elapsed().as_nanos() as u64);
            }
            let s = summarize(samples);
            seqpacket_p50.insert(payload, s.p50);
            println!(
                "  {:>8}  {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>9.2}",
                payload, s.p50, s.p90, s.p99, s.p999, s.max, s.mean
            );
            if payload == 8 {
                dists.push(("raw seqpacket, 8 B".to_string(), s.samples));
            }
        }
    }

    // ---- latency, data only
    header("round-trip latency — data only (µs, one transaction in flight)");
    println!(
        "  {:>8}  {:>8} {:>8} {:>8} {:>8} {:>8} {:>9}  {:>9}",
        "payload", "p50", "p90", "p99", "p99.9", "max", "mean", "cpu/txn"
    );
    for payload in [8usize, 64, 512, 1024, 4096, 8192] {
        let (s, u) = client.latency(payload, 0, 0, args.iters);
        let cpu_us = (u.client_cpu + u.server_cpu) * 1e6 / args.iters as f64;
        // measured against the raw binder floor when we have one — that difference is
        // the library's cost. the seqpacket number is a different transport and is not
        // subtracted from anything
        let vs_raw = match binder_p50.get(&payload) {
            Some(r) => format!("  {:+.1}µs vs raw binder", s.p50 - r),
            None => String::new(),
        };
        println!(
            "  {:>8}  {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>9.2}  {:>7.2}µs{vs_raw}",
            payload, s.p50, s.p90, s.p99, s.p999, s.max, s.mean, cpu_us
        );
        if payload == 8 {
            dists.push(("binder, 8 B, data only".to_string(), s.samples));
        }
    }

    // ---- latency, one binder ref per transaction
    header("round-trip latency — one binder ref per transaction (µs)");
    println!(
        "  {:>8}  {:>8} {:>8} {:>8} {:>8} {:>8} {:>9}  {:>9}",
        "payload", "p50", "p90", "p99", "p99.9", "max", "mean", "cpu/txn"
    );
    for payload in [8usize, 1024, 8192] {
        let (s, u) = client.latency(payload, 1, 0, args.iters);
        let cpu_us = (u.client_cpu + u.server_cpu) * 1e6 / args.iters as f64;
        println!(
            "  {:>8}  {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>9.2}  {:>7.2}µs",
            payload, s.p50, s.p90, s.p99, s.p999, s.max, s.mean, cpu_us
        );
        if payload == 8 {
            dists.push(("binder, 8 B, one binder ref".to_string(), s.samples));
        }
    }

    // ---- latency, one fd per transaction
    header("round-trip latency — one fd per transaction (µs)");
    println!(
        "  {:>8}  {:>8} {:>8} {:>8} {:>8} {:>8} {:>9}  {:>9}",
        "payload", "p50", "p90", "p99", "p99.9", "max", "mean", "cpu/txn"
    );
    for payload in [8usize, 1024, 8192] {
        let (s, u) = client.latency(payload, 0, 1, args.iters);
        let cpu_us = (u.client_cpu + u.server_cpu) * 1e6 / args.iters as f64;
        println!(
            "  {:>8}  {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>9.2}  {:>7.2}µs",
            payload, s.p50, s.p90, s.p99, s.p999, s.max, s.mean, cpu_us
        );
        if payload == 8 {
            dists.push(("binder, 8 B, one fd".to_string(), s.samples));
        }
    }

    // ---- capability batches
    let batch_iters = (args.iters / 4).max(500);
    for (what, refs, fds) in [("binder refs", true, false), ("fds", false, true)] {
        header(&format!(
            "round-trip latency — {what} per transaction, 8 B payload (µs)"
        ));
        println!(
            "  {:>8}  {:>8} {:>8} {:>8} {:>8}  {:>9}  {:>9}",
            "per txn", "p50", "p90", "p99", "max", "mean", "cpu/txn"
        );
        for n in [1usize, 8, 32, 128, 253] {
            let (s, u) = client.latency(
                8,
                if refs { n } else { 0 },
                if fds { n } else { 0 },
                batch_iters,
            );
            let cpu_us = (u.client_cpu + u.server_cpu) * 1e6 / batch_iters as f64;
            println!(
                "  {:>8}  {:>8.2} {:>8.2} {:>8.2} {:>8.2}  {:>9.2}  {:>7.2}µs",
                n, s.p50, s.p90, s.p99, s.max, s.mean, cpu_us
            );
            println!(
                "            peak fds during phase — client {:>6}  service {:>6}",
                watch.peak_self.load(Ordering::Relaxed),
                watch.peak_child.load(Ordering::Relaxed)
            );
            if n == 32 {
                dists.push((format!("binder, 8 B, 32 {what}"), s.samples));
            }
        }
    }

    // ---- the shape behind the percentiles
    header("latency distributions (√2-spaced buckets)");
    for (title, samples) in &dists {
        histogram(title, samples);
        println!();
    }

    // ---- throughput, blocking round trips
    header(&format!(
        "throughput — blocking round trips, {} threads, {} s per row",
        args.jobs, args.secs
    ));
    println!(
        "  {:>8} {:>5} {:>4}  {:>11} {:>12}  {:>8} {:>8}  {:>8} {:>10}",
        "payload", "refs", "fds", "txn/s", "goodput", "cli cpu", "svc cpu", "failed", "pk fds svc"
    );
    for (payload, refs, fds) in [
        (8usize, 0usize, 0usize),
        (1024, 0, 0),
        (8192, 0, 0),
        (8, 1, 0),
        (1024, 1, 0),
        (8, 0, 1),
        (1024, 0, 1),
    ] {
        let (done, failed, u) = client.throughput_blocking(
            payload,
            refs,
            fds,
            args.jobs,
            Duration::from_secs(args.secs),
        );
        let rate = done as f64 / u.wall;
        println!(
            "  {:>8} {:>5} {:>4}  {:>11.0} {:>12}  {:>7.0}% {:>7.0}%  {:>8} {:>10}",
            payload,
            refs,
            fds,
            rate,
            fmt_bytes_per_sec(rate * payload as f64),
            u.client_cpu / u.wall * 100.0,
            u.server_cpu / u.wall * 100.0,
            failed,
            watch.peak_child.load(Ordering::Relaxed)
        );
    }
    println!("  cpu percentages are of one core, so >100% means several loopers were busy");
    println!("  at once. the service caps out at its looper count, not at the client's.");

    // ---- throughput, one-way
    header(&format!(
        "throughput — one-way (no reply), single sender, {} s per row",
        args.secs
    ));
    println!(
        "  {:>8} {:>5}  {:>11} {:>12}  {:>8} {:>8}  {:>9} {:>9}",
        "payload", "refs", "handled/s", "goodput", "cli cpu", "svc cpu", "backpres", "lost"
    );
    for (payload, refs) in [(8usize, 0usize), (1024, 0), (8192, 0), (8, 1)] {
        let (sent, handled, rejected, u) =
            client.throughput_one_way(payload, refs, Duration::from_secs(args.secs));
        let rate = handled as f64 / u.wall;
        println!(
            "  {:>8} {:>5}  {:>11.0} {:>12}  {:>7.0}% {:>7.0}%  {:>8.1}% {:>9}",
            payload,
            refs,
            rate,
            fmt_bytes_per_sec(rate * payload as f64),
            u.client_cpu / u.wall * 100.0,
            u.server_cpu / u.wall * 100.0,
            rejected as f64 / (sent + rejected).max(1) as f64 * 100.0,
            sent.saturating_sub(handled),
        );
    }
    println!("  'backpres' is the share of one-way sends the kernel refused because the");
    println!("  service's async buffer (half its mapping) was full; 'lost' is anything sent");
    println!("  that never showed up in the service's own counter.");

    // ---- descriptor and refcount churn
    header(&format!(
        "capability churn — {} s of ref + fd passing at full rate",
        args.churn
    ));
    {
        *watch.timeline.lock().unwrap() = Some((Instant::now(), Vec::new()));
        let (done, failed, u) =
            client.throughput_blocking(64, 1, 1, args.jobs, Duration::from_secs(args.churn));
        let log = watch.timeline.lock().unwrap().take().unwrap().1;

        println!("  transactions      {done} completed, {failed} failed");
        println!("  rate              {:.0} txn/s", done as f64 / u.wall);
        println!();
        println!(
            "  {:>6}  {:>12} {:>12} {:>10}",
            "t (s)", "client rss", "service rss", "svc fds"
        );
        for (t, c, s, f) in log.iter().step_by(3) {
            println!("  {t:>6.0}  {c:>8} KiB {s:>8} KiB {f:>10}");
        }
        println!();
        println!(
            "  peak fds          client {}   service {}",
            watch.peak_self.load(Ordering::Relaxed),
            watch.peak_child.load(Ordering::Relaxed)
        );
        // give both sides a moment to finish dropping whatever was in flight
        std::thread::sleep(Duration::from_millis(500));
        println!(
            "  fds at rest       client {}   service {}",
            fd_count(self_pid),
            fd_count(child_pid)
        );
        println!(
            "  rss at rest       client {} KiB   service {} KiB",
            rss_kb(self_pid),
            rss_kb(child_pid)
        );
        println!();
        println!("  flat 'at rest' numbers mean every passed ref and descriptor was reclaimed;");
        println!("  a rising service fd column means descriptors outrun the drops, and a rising");
        println!("  rss column means refcounts (or their bookkeeping) are leaking.");
    }

    // ---- payload ceiling
    header("payload ceiling");
    println!("  each side maps 1 MiB of binder buffer, and a two-way transaction needs room");
    println!("  for the request in the service's mapping and the reply in the client's — so");
    println!("  the practical ceiling is well under the mapping size.");
    println!();
    println!(
        "  each probe runs on a throwaway thread with a {PROBE_TIMEOUT_SECS} s deadline: a transaction"
    );
    println!("  the kernel refuses can leave `transact_blocking` parked in binder_wait_for_work");
    println!("  forever, and a benchmark that wedges there reports nothing at all.");
    for payload in [
        4096usize,
        65536,
        262144,
        524288,
        1024 * 1024,
        2 * 1024 * 1024,
    ] {
        match probe_payload(&client, payload) {
            Some(Ok(n)) if n == payload => println!("  {payload:>8} B  → echoed intact"),
            Some(Ok(n)) => println!("  {payload:>8} B  → came back as {n} B — TRUNCATED"),
            Some(Err(e)) => println!("  {payload:>8} B  → refused: {e:?}"),
            None => {
                println!(
                    "  {payload:>8} B  → NO REPLY within {PROBE_TIMEOUT_SECS} s — the sender is still blocked"
                );
                println!("            (skipping the larger sizes; that thread never comes back)");
                break;
            }
        }
    }

    // ---- final resource summary
    header("resource summary");
    println!(
        "  client   rss {:>7} KiB   peak rss {:>7} KiB   cpu {:>6.2} s   fds {:>5}",
        rss_kb(self_pid),
        peak_rss_kb(self_pid),
        cpu_seconds(self_pid).unwrap_or(0.0),
        fd_count(self_pid)
    );
    println!(
        "  service  rss {:>7} KiB   peak rss {:>7} KiB   cpu {:>6.2} s   fds {:>5}",
        rss_kb(child_pid),
        peak_rss_kb(child_pid),
        cpu_seconds(child_pid).unwrap_or(0.0),
        fd_count(child_pid)
    );
    println!(
        "  growth   client rss +{} KiB   service rss +{} KiB   (vs idle)",
        rss_kb(self_pid) as i64 - idle_client_rss as i64,
        rss_kb(child_pid) as i64 - idle_service_rss as i64
    );

    shutdown(child, path);
    println!();
}

/// how long a payload-ceiling probe waits before writing the transaction off
const PROBE_TIMEOUT_SECS: u64 = 5;

/// one oversized-payload probe on a detached thread
///
/// `Some(Ok(bytes))` echoed, `Some(Err(_))` refused, `None` still blocked when the
/// deadline passed — the thread is abandoned in that case, which is fine because the
/// only thing left to do afterwards is read /proc and exit.
fn probe_payload(
    client: &Arc<Client>,
    payload: usize,
) -> Option<std::result::Result<usize, binderbinder::Error>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let client = client.clone();
    let handle = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        let _guard = handle.enter();
        let data = vec![0x41u8; payload];
        // the 8-byte header rides along with every bench payload
        let result = client
            .round_trip(&data, 0, 0)
            .map(|reply| reply.bytes_until_next_obj().saturating_sub(8));
        let _ = tx.send(result);
    });
    rx.recv_timeout(Duration::from_secs(PROBE_TIMEOUT_SECS))
        .ok()
}

fn shutdown(child: Pid, _path: PathBuf) {
    let _ = nix::sys::signal::kill(child, nix::sys::signal::Signal::SIGKILL);
    let _ = nix::sys::wait::waitpid(child, None);
}
