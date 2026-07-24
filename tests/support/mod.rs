//! Shared fixture for binderbinder's integration test suite.
//!
//! Prerequisite (once per machine/CI image, as root):
//!   `sudo cargo run --example setup_test_pool`
//! That provisions a pool of pre-created, world-writable binderfs device
//! nodes. Everything in this module runs as a normal user and only ever
//! opens/locks nodes from that pool — no privileged calls happen in tests.
//!
//! Two flavors of test are supported:
//!   - single-process (e.g. local self-transact): just call
//!     `PoolNode::acquire()` and use `BinderDevice` directly under
//!     `#[tokio::test]`.
//!   - multi-process (a real context-manager/service process talking to a
//!     real separate client process): use `fork_combo`, which does a real
//!     `fork()` early (before any tokio runtime exists) so each role gets
//!     its own OS process, its own `binder_proc` in the kernel, and its own
//!     `tokio::runtime::Runtime` — matching how binder is actually used, and
//!     how the kernel's own binderfs selftests test it
//!     (`tools/testing/selftests/filesystems/binderfs/binderfs_test.c`).

use std::future::Future;
use std::sync::Arc;

use binderbinder::BinderDevice;
pub use binderbinder::test_pool::PoolNode;

use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{fork, pipe, read, write, ForkResult, Pid};

/// Result of a `fork_combo` run: the client role's return value, plus the
/// forked service process's exit status (`WaitStatus::Exited(_, 0)` is
/// success — anything else means the service role panicked or was killed).
pub struct ComboResult<T> {
    #[allow(dead_code, reason = "not every combo test needs the client role's return value")]
    pub client: T,
    pub child_status: WaitStatus,
}

/// Runs `service_role` in a real forked child (becoming context manager and
/// registering whatever handler it likes on `node`'s device), and
/// `client_role` in the current process against the same device node, once
/// the service has signaled readiness. Returns once the client role
/// completes and the child has been reaped.
///
/// `service_role` must return a value that keeps its registered object(s)
/// alive (e.g. the `BinderObject<T>` guard from `register_object`) — it is
/// held in the child for the combo's whole lifetime and only dropped right
/// before the child exits.
///
/// Must be called before this test process has spun up any tokio runtime of
/// its own — `fork()` after a multi-threaded runtime is running is unsound.
pub fn fork_combo<K, ServiceFut, ClientFut, T>(
    node: &PoolNode,
    service_role: impl FnOnce(Arc<BinderDevice>) -> ServiceFut + 'static,
    client_role: impl FnOnce(Arc<BinderDevice>) -> ClientFut + 'static,
) -> ComboResult<T>
where
    ServiceFut: Future<Output = K>,
    ClientFut: Future<Output = T>,
{
    let path = node.path.clone();

    // ready_r/ready_w: child -> parent, "I'm registered and am the context
    // manager". done_r/done_w: parent -> child, "I'm finished transacting,
    // you can exit". Both pipes are created before fork so each end is
    // inherited by both processes; each side immediately drops the end it
    // doesn't own.
    let (ready_r, ready_w) = pipe().expect("pipe (ready)");
    let (done_r, done_w) = pipe().expect("pipe (done)");

    match unsafe { fork() }.expect("fork failed") {
        ForkResult::Child => {
            drop(ready_r);
            drop(done_w);

            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let rt = tokio::runtime::Runtime::new().expect("build child runtime");
                let keep_alive = rt.block_on(async {
                    let device = BinderDevice::new(&path).expect("open device (service role)");
                    service_role(device).await
                });

                // Signal readiness only after setup succeeded, then block
                // until the client is done. `keep_alive` (e.g. the
                // registered BinderObject) stays alive on this stack frame
                // for as long as we're blocked here.
                write(&ready_w, &[1]).expect("signal ready");
                let mut buf = [0u8; 1];
                let _ = read(&done_r, &mut buf);
                drop(keep_alive);
            }));

            // Make sure the parent never blocks forever on a service that
            // panicked before it could signal readiness.
            let _ = write(&ready_w, &[0]);
            std::process::exit(if result.is_ok() { 0 } else { 1 });
        }
        ForkResult::Parent { child } => {
            drop(ready_w);
            drop(done_r);

            let mut buf = [0u8; 1];
            read(&ready_r, &mut buf).expect("read ready signal");
            assert_eq!(buf[0], 1, "service role failed before becoming ready");
            drop(ready_r);

            let rt = tokio::runtime::Runtime::new().expect("build parent runtime");
            let client = rt.block_on(async {
                let device = BinderDevice::new(&path).expect("open device (client role)");
                client_role(device).await
            });

            write(&done_w, &[1]).expect("signal done");
            drop(done_w);

            let child_status = waitpid(child, None).expect("waitpid");
            ComboResult {
                client,
                child_status,
            }
        }
    }
}

/// A forked service role with no built-in "done" handshake — for
/// death-notification tests, where the point is for the caller to kill the
/// child mid-flight rather than have it exit cleanly.
pub struct ForkedService {
    pub pid: Pid,
}

/// Like `fork_combo`'s child branch, but the caller controls the child's
/// lifetime directly (via `kill_child` + `reap`) instead of a done-signal
/// handshake. Blocks until the service signals readiness.
pub fn fork_service<K, ServiceFut>(
    node: &PoolNode,
    service_role: impl FnOnce(Arc<BinderDevice>) -> ServiceFut + 'static,
) -> ForkedService
where
    ServiceFut: Future<Output = K>,
{
    let path = node.path.clone();
    let (ready_r, ready_w) = pipe().expect("pipe (ready)");

    match unsafe { fork() }.expect("fork failed") {
        ForkResult::Child => {
            drop(ready_r);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let rt = tokio::runtime::Runtime::new().expect("build child runtime");
                let keep_alive = rt.block_on(async {
                    let device = BinderDevice::new(&path).expect("open device (service role)");
                    service_role(device).await
                });
                write(&ready_w, &[1]).expect("signal ready");
                // Park here (rather than exit) so the object stays
                // registered until the parent kills us.
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(3600));
                    std::hint::black_box(&keep_alive);
                }
            }));
            let _ = write(&ready_w, &[0]);
            std::process::exit(if result.is_ok() { 0 } else { 1 });
        }
        ForkResult::Parent { child } => {
            let mut buf = [0u8; 1];
            read(&ready_r, &mut buf).expect("read ready signal");
            assert_eq!(buf[0], 1, "service role failed before becoming ready");
            ForkedService { pid: child }
        }
    }
}

/// Sends `SIGKILL` to a forked service role's process — for death-notification
/// tests, this is a strictly more realistic "the remote process died" event
/// than dropping an `Arc` in-process.
pub fn kill_child(pid: Pid) {
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL).expect("kill child");
}

/// Reaps a child previously started with `fork_service`, e.g. after
/// `kill_child`.
pub fn reap(pid: Pid) -> WaitStatus {
    waitpid(pid, None).expect("waitpid")
}
