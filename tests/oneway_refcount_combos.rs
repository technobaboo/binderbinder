//! Cross-process strong-refcount stress tests for *oneway* (asynchronous)
//! transactions specifically.
//!
//! `tests/refcount_combos.rs` covers the *service hands a fresh object to
//! the client via a two-way BC_REPLY* direction (which had a real bug, fixed
//! in `src/device.rs`'s looper: BC_REPLY was sent via a read-less ioctl,
//! which could starve the BR_ACQUIRE the kernel queues onto that same
//! thread's own work list as a side effect of translating the embedded
//! object).
//!
//! This file exercises the *other* direction: the **client** owns a fresh
//! object and hands a ref to it to the service as an **argument in a oneway
//! `BC_TRANSACTION`** (not a reply). That embedding goes through the exact
//! same kernel `binder_translate_binder` mechanism (queuing a BR_ACQUIRE
//! onto the sending thread's own work list), but on a different code path:
//! `remote_transact_one_way` (already using the combined write+read
//! `binder_write_read`, unlike the old `write_binder_command` reply path) on
//! the send side, and `handle_one_way`/`PayloadReader::drop` on the receive
//! side, where a received object can be consumed explicitly
//! (`read_binder_ref`) or dropped implicitly by never reading it.

mod support;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use binderbinder::binder_object::{BinderObject, BinderObjectRef};
use binderbinder::device::Transaction;
use binderbinder::payload::PayloadBuilder;
use binderbinder::{BinderDevice, TransactionHandler};

use support::PoolNode;

const DROP_IMMEDIATELY_CODE: u32 = 1;
const HOLD_THEN_DROP_CODE: u32 = 2;

/// A trivial object with nothing behind it — its only role is to have a
/// strong-refcount lifecycle to observe.
#[derive(Debug)]
struct Leaf;
impl TransactionHandler for Leaf {
    async fn handle(self: Arc<Self>, _tx: Transaction) -> PayloadBuilder<'static> {
        PayloadBuilder::new()
    }
    async fn handle_one_way(self: Arc<Self>, _tx: Transaction) {}
}

/// Registered as the context manager. Receives a ref embedded as an
/// *argument* of an incoming oneway transaction (never as part of a reply —
/// oneway has no reply) and either drops it immediately or holds it for a
/// while first, per the code.
#[derive(Debug, Default)]
struct Sink;
impl TransactionHandler for Sink {
    async fn handle(self: Arc<Self>, _tx: Transaction) -> PayloadBuilder<'static> {
        PayloadBuilder::new()
    }
    async fn handle_one_way(self: Arc<Self>, mut transaction: Transaction) {
        match transaction.code {
            DROP_IMMEDIATELY_CODE => {
                // Do nothing: falling off the end drops `transaction.payload`
                // untouched. `PayloadReader::drop` drains (and drops) any
                // embedded object it never got explicitly read out — the
                // fastest possible receive-then-release.
            }
            HOLD_THEN_DROP_CODE => {
                let obj = transaction
                    .payload
                    .read_binder_ref()
                    .expect("expected an embedded ref");
                tokio::time::sleep(Duration::from_millis(200)).await;
                drop(obj);
            }
            _ => {}
        }
    }
}

async fn become_sink(device: Arc<BinderDevice>) -> BinderObject<Sink> {
    let obj = device.register_object(Sink);
    device
        .set_context_manager(&obj)
        .await
        .expect("set_context_manager (service role)");
    obj
}

/// Creates a fresh `Leaf`, hands a ref to it to the sink as the argument of
/// a oneway transaction, and returns the local guard — the caller decides
/// when to drop it and observes `strong_refs_hit_zero()` directly against
/// *this* (the owning) process's own device, since that's where the real
/// bookkeeping for an object this process owns lives (no need to poll the
/// remote side the way the two-way tests have to).
async fn create_and_send(device: &Arc<BinderDevice>, code: u32) -> BinderObjectRef<Leaf> {
    let leaf = device.register_object(Leaf);
    let leaf_ref = leaf.to_service();

    let device2 = device.clone();
    let leaf_ref2 = leaf_ref.clone();
    tokio::task::spawn_blocking(move || {
        let mut payload = PayloadBuilder::new();
        payload.push_binder_ref(&leaf_ref2);
        device2.transact_one_way(device2.context_manager(), code, payload)
    })
    .await
    .unwrap()
    .expect("oneway send failed");

    leaf_ref
}

/// The sink receives the ref and drops it (implicitly, via an unread
/// `PayloadReader`) almost immediately — the fast send-then-release path
/// most likely to race the kernel's BR_ACQUIRE against our own pending-remote
/// bookkeeping, now via a oneway argument instead of a two-way reply.
#[test]
fn oneway_dropped_ref_releases_promptly() {
    let node = PoolNode::acquire();
    let result = support::fork_combo(
        &node,
        become_sink,
        |device: Arc<BinderDevice>| async move {
            let leaf_ref = create_and_send(&device, DROP_IMMEDIATELY_CODE).await;
            let hit_zero = leaf_ref.strong_refs_hit_zero();
            drop(leaf_ref);
            tokio::time::timeout(Duration::from_secs(2), hit_zero)
                .await
                .expect("strong_refs_hit_zero did not fire within 2s");
        },
    );
    assert!(matches!(
        result.child_status,
        nix::sys::wait::WaitStatus::Exited(_, 0)
    ));
}

/// The sink holds the received ref for a while — as long as *we* (the
/// owning process) still hold our own local ref too, `strong_refs_hit_zero`
/// must not fire, then must fire shortly after we drop ours.
#[test]
fn oneway_held_ref_delays_release_until_dropped() {
    let node = PoolNode::acquire();
    let result = support::fork_combo(
        &node,
        become_sink,
        |device: Arc<BinderDevice>| async move {
            let leaf_ref = create_and_send(&device, HOLD_THEN_DROP_CODE).await;
            let mut hit_zero = Box::pin(leaf_ref.strong_refs_hit_zero());

            // Poll (without consuming) a few times while the sink is still
            // holding its own ref — must stay Pending regardless of what the
            // sink does, since *we* still hold ours too.
            let waker = std::task::Waker::noop().clone();
            let mut cx = std::task::Context::from_waker(&waker);
            for _ in 0..5 {
                tokio::time::sleep(Duration::from_millis(20)).await;
                assert!(
                    hit_zero.as_mut().poll(&mut cx).is_pending(),
                    "strong_refs_hit_zero fired while we still held our own ref"
                );
            }

            drop(leaf_ref);
            tokio::time::timeout(Duration::from_secs(2), hit_zero)
                .await
                .expect("strong_refs_hit_zero did not fire within 2s");
        },
    );
    assert!(matches!(
        result.child_status,
        nix::sys::wait::WaitStatus::Exited(_, 0)
    ));
}

/// Many concurrent create+oneway-send+drop cycles against the same sink, to
/// stress the refcount bookkeeping under oneway contention specifically
/// (mirrors `refcount_combos::concurrent_create_and_drop_cycles`, but for
/// the oneway-argument direction instead of the two-way-reply direction).
#[test]
fn oneway_concurrent_create_and_drop_cycles() {
    let node = PoolNode::acquire();
    let result = support::fork_combo(
        &node,
        become_sink,
        |device: Arc<BinderDevice>| async move {
            let mut tasks = Vec::new();
            for _ in 0..16 {
                let device = device.clone();
                tasks.push(tokio::spawn(async move {
                    let leaf_ref = create_and_send(&device, DROP_IMMEDIATELY_CODE).await;
                    let hit_zero = leaf_ref.strong_refs_hit_zero();
                    drop(leaf_ref);
                    tokio::time::timeout(Duration::from_secs(5), hit_zero)
                        .await
                        .expect("strong_refs_hit_zero stuck");
                }));
            }
            for t in tasks {
                t.await.unwrap();
            }
        },
    );
    assert!(matches!(
        result.child_status,
        nix::sys::wait::WaitStatus::Exited(_, 0)
    ));
}

/// Same object, sent via oneway repeatedly without ever dropping our own
/// local ref in between — exercises the *not-first-ref* path (the node
/// already has a strong ref in the target process, so the kernel doesn't
/// queue a fresh BR_ACQUIRE on each send) concurrently with fresh-object
/// sends from other tasks, then drains everything at the end.
#[test]
fn oneway_repeated_sends_of_same_object() {
    let node = PoolNode::acquire();
    let result = support::fork_combo(
        &node,
        become_sink,
        |device: Arc<BinderDevice>| async move {
            let leaf_ref = create_and_send(&device, DROP_IMMEDIATELY_CODE).await;
            let hit_zero = leaf_ref.strong_refs_hit_zero();

            let mut tasks = Vec::new();
            for _ in 0..16 {
                let device = device.clone();
                let leaf_ref2 = leaf_ref.clone();
                tasks.push(tokio::spawn(async move {
                    tokio::task::spawn_blocking(move || {
                        let mut payload = PayloadBuilder::new();
                        payload.push_binder_ref(&leaf_ref2);
                        device.transact_one_way(
                            device.context_manager(),
                            DROP_IMMEDIATELY_CODE,
                            payload,
                        )
                    })
                    .await
                    .unwrap()
                    .expect("oneway send failed");
                }));
            }
            for t in tasks {
                t.await.unwrap();
            }

            drop(leaf_ref);
            tokio::time::timeout(Duration::from_secs(5), hit_zero)
                .await
                .expect("strong_refs_hit_zero stuck");
        },
    );
    assert!(matches!(
        result.child_status,
        nix::sys::wait::WaitStatus::Exited(_, 0)
    ));
}
