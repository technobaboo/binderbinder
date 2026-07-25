//! Cross-process strong-refcount stress tests: a service hands a *fresh*
//! object to a real remote client over an actual transaction (going through
//! the kernel's BR_ACQUIRE/BR_RELEASE dance, not the in-process `Local` fast
//! path), and we assert the service only sees `strong_refs_hit_zero` once
//! the remote side has genuinely let go — whether that's almost
//! immediately, or after holding the ref for a while first. This is the
//! kind of race the in-process refcount unit tests in `src/device.rs` can't
//! reach, since a same-process self-transaction never touches the kernel's
//! object refcounting at all.

mod support;

use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use binderbinder::binder_object::BinderObjectOrRef;
use binderbinder::device::Transaction;
use binderbinder::payload::PayloadBuilder;
use binderbinder::{BinderDevice, TransactionHandler};

use support::PoolNode;

const CREATE_CODE: u32 = 1;
const STATUS_CODE: u32 = 2;

/// A trivial object with nothing behind it — its only role here is to have
/// a strong-refcount lifecycle to observe.
#[derive(Debug)]
struct Leaf;
impl TransactionHandler for Leaf {
    async fn handle(self: Arc<Self>, _tx: Transaction) -> PayloadBuilder<'static> {
        PayloadBuilder::new()
    }
    async fn handle_one_way(self: Arc<Self>, _tx: Transaction) {}
}

/// Registered as the context manager. Each `CREATE_CODE` registers a fresh
/// `Leaf`, hands a ref to it back in the reply (without keeping one for
/// itself), and bumps `hit_zero_count` once that particular `Leaf`'s strong
/// refcount hits zero, so `STATUS_CODE` can report the running total back
/// to the client — a counter rather than a flag, so concurrent create/drop
/// cycles against many distinct objects can all be accounted for.
#[derive(Debug, Default)]
struct RefFactoryService {
    device: OnceLock<Arc<BinderDevice>>,
    hit_zero_count: AtomicU32,
}

impl TransactionHandler for RefFactoryService {
    async fn handle(self: Arc<Self>, transaction: Transaction) -> PayloadBuilder<'static> {
        let mut builder = PayloadBuilder::new();
        match transaction.code {
            CREATE_CODE => {
                let device = self
                    .device
                    .get()
                    .expect("device set before serving any transactions")
                    .clone();
                let leaf = device.register_object(Leaf);
                let leaf_ref = leaf.to_service();

                let this = self.clone();
                let mut fires = Box::pin(leaf_ref.strong_refs_hit_zero());
                // `notify_waiters()` only wakes waiters that are already
                // registered at the moment it fires — it doesn't buffer a
                // notification for a task that hasn't polled yet. A plain
                // `tokio::spawn(async move { fires.await; ... })` here would
                // leave a window where the spawned task hasn't had its
                // first poll yet when the real notify fires, and would then
                // hang forever (the same gotcha the in-process unit tests
                // in `src/device.rs` have to work around). Poll it once,
                // synchronously, right here — this is exactly what the
                // first `.await` inside that spawned task would have done,
                // just not left to the scheduler's discretion — so the
                // `Notify` waiter is unconditionally registered before we
                // hand the object off and this transaction finishes.
                let waker = std::task::Waker::noop().clone();
                let mut cx = std::task::Context::from_waker(&waker);
                if fires.as_mut().poll(&mut cx).is_pending() {
                    tokio::spawn(async move {
                        fires.await;
                        this.hit_zero_count.fetch_add(1, Ordering::SeqCst);
                    });
                } else {
                    this.hit_zero_count.fetch_add(1, Ordering::SeqCst);
                }

                // Hand the only reference we have straight to the reply —
                // we never keep a local BinderObjectRef of our own, so once
                // this returns, the object's fate rests entirely on what
                // the remote side (the client) does with the ref it gets.
                builder.push_binder_ref(&leaf_ref);
            }
            STATUS_CODE => {
                builder.push_bytes(&self.hit_zero_count.load(Ordering::SeqCst).to_ne_bytes());
            }
            _ => {}
        }
        builder
    }

    async fn handle_one_way(self: Arc<Self>, _transaction: Transaction) {}
}

async fn become_ref_factory(
    device: Arc<BinderDevice>,
) -> binderbinder::binder_object::BinderObject<RefFactoryService> {
    let handler = Arc::new(RefFactoryService::default());
    let obj = device.register_object(handler.clone());
    device
        .set_context_manager(&obj)
        .await
        .expect("set_context_manager (service role)");
    handler.device.set(device).expect("device set exactly once");
    obj
}

async fn create_ref(device: &Arc<BinderDevice>) -> Arc<binderbinder::binder_object::BinderRef> {
    let device = device.clone();
    let (_, mut reply) = tokio::task::spawn_blocking(move || {
        let payload = PayloadBuilder::new();
        device.transact_blocking(device.context_manager(), CREATE_CODE, payload)
    })
    .await
    .unwrap()
    .expect("create transaction failed");

    match reply
        .read_binder_ref()
        .expect("expected a binder ref in the reply")
    {
        BinderObjectOrRef::Ref(r) => r,
        other => panic!("expected a remote handle, got {other:?}"),
    }
}

async fn hit_zero_count(device: &Arc<BinderDevice>) -> u32 {
    let device = device.clone();
    let (_, mut reply) = tokio::task::spawn_blocking(move || {
        let payload = PayloadBuilder::new();
        device.transact_blocking(device.context_manager(), STATUS_CODE, payload)
    })
    .await
    .unwrap()
    .expect("status transaction failed");
    u32::from_ne_bytes(reply.read_bytes(4).unwrap().try_into().unwrap())
}

async fn wait_until_hit_zero_count(device: &Arc<BinderDevice>, at_least: u32, timeout: Duration) {
    let mut last = 0;
    let result = tokio::time::timeout(timeout, async {
        loop {
            last = hit_zero_count(device).await;
            if last >= at_least {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    if result.is_err() {
        panic!(
            "strong_refs_hit_zero count stuck at {last}, never reached {at_least} within timeout"
        );
    }
}

/// The client gets a ref and drops it almost immediately without ever
/// holding onto it "for a while" — this is the fast send-then-release path
/// that's most likely to race the kernel's BR_ACQUIRE against our local
/// bookkeeping (`mark_pending_remote`/`clear_pending_remote` in
/// `src/device.rs`).
#[test]
fn dropped_ref_releases_promptly() {
    let node = PoolNode::acquire();
    let result = support::fork_combo(
        &node,
        become_ref_factory,
        |device: Arc<BinderDevice>| async move {
            let leaf_ref = create_ref(&device).await;
            drop(leaf_ref);
            wait_until_hit_zero_count(&device, 1, Duration::from_secs(2)).await;
        },
    );
    assert!(matches!(
        result.child_status,
        nix::sys::wait::WaitStatus::Exited(_, 0)
    ));
}

/// The client holds the ref for a while, during which the service must
/// *not* see the refcount hit zero, then drops it and the service must see
/// it hit zero shortly after — proving the remote acquire genuinely keeps
/// the object alive rather than the service-side guard being what was
/// holding it.
#[test]
fn held_ref_delays_release_until_dropped() {
    let node = PoolNode::acquire();
    let result = support::fork_combo(
        &node,
        become_ref_factory,
        |device: Arc<BinderDevice>| async move {
            let leaf_ref = create_ref(&device).await;

            for _ in 0..10 {
                tokio::time::sleep(Duration::from_millis(20)).await;
                assert_eq!(
                    hit_zero_count(&device).await,
                    0,
                    "service saw strong_refs_hit_zero while the client still held the ref"
                );
            }

            drop(leaf_ref);
            wait_until_hit_zero_count(&device, 1, Duration::from_secs(2)).await;
        },
    );
    assert!(matches!(
        result.child_status,
        nix::sys::wait::WaitStatus::Exited(_, 0)
    ));
}

/// Many concurrent create/drop cycles against the same service, to stress
/// the refcount bookkeeping under contention rather than one clean
/// request at a time.
#[test]
fn concurrent_create_and_drop_cycles() {
    let node = PoolNode::acquire();
    let result = support::fork_combo(
        &node,
        become_ref_factory,
        |device: Arc<BinderDevice>| async move {
            let mut tasks = Vec::new();
            for _ in 0..16 {
                let device = device.clone();
                tasks.push(tokio::spawn(async move {
                    let leaf_ref = create_ref(&device).await;
                    drop(leaf_ref);
                }));
            }
            for t in tasks {
                t.await.unwrap();
            }
            wait_until_hit_zero_count(&device, 16, Duration::from_secs(5)).await;
        },
    );
    assert!(matches!(
        result.child_status,
        nix::sys::wait::WaitStatus::Exited(_, 0)
    ));
}
