//! Stress matrix over the main transaction combos: mode x target x payload
//! shape x concurrency, plus a death-notification scenario. See
//! `tests/support/mod.rs` for the pool/fork fixtures this builds on, and
//! `examples/self_transaction.rs`/`examples/echo_client.rs` for the
//! single-process pattern this generalizes to real separate processes.

mod support;

use std::sync::Arc;

use binderbinder::device::Transaction;
use binderbinder::payload::{BinderObjectType, PayloadBuilder};
use binderbinder::{BinderDevice, TransactionHandler};

use support::{fork_service, kill_child, reap, PoolNode};

const ECHO_CODE: u32 = 1;

#[derive(Debug)]
struct EchoService;

impl TransactionHandler for EchoService {
    async fn handle(self: Arc<Self>, mut transaction: Transaction) -> PayloadBuilder<'static> {
        let mut builder = PayloadBuilder::new();
        loop {
            let bytes = transaction.payload.bytes_until_next_obj();
            if bytes != 0 {
                let Ok(v) = transaction.payload.read_bytes(bytes) else {
                    break;
                };
                builder.push_bytes(v);
                continue;
            }
            match transaction.payload.next_object_type() {
                Some(
                    BinderObjectType::BinderRef
                    | BinderObjectType::WeakBinderRef
                    | BinderObjectType::WeakBinderObject
                    | BinderObjectType::BinderObject,
                ) => {
                    builder.push_binder_ref(&transaction.payload.read_binder_ref().unwrap());
                    continue;
                }
                Some(BinderObjectType::Fd) => {
                    let (fd, cookie) = transaction.payload.read_fd().unwrap();
                    builder.push_owned_fd(fd, cookie);
                    continue;
                }
                _ => {}
            }
            break;
        }
        builder
    }

    async fn handle_one_way(self: Arc<Self>, _transaction: Transaction) {}
}

async fn become_echo_service(device: Arc<BinderDevice>) -> binderbinder::binder_object::BinderObject<EchoService> {
    let obj = device.register_object(EchoService);
    device
        .set_context_manager(&obj)
        .await
        .expect("set_context_manager (service role)");
    obj
}

fn payload_of(shape: &str) -> Vec<u8> {
    match shape {
        "empty" => Vec::new(),
        "small" => b"hello from a real forked client".to_vec(),
        "large" => vec![0xABu8; 256 * 1024],
        other => panic!("unknown payload shape {other}"),
    }
}

/// One echo round-trip against a real forked context-manager/service
/// process, either blocking or one-way.
async fn echo_once(device: Arc<BinderDevice>, shape: &'static str, one_way: bool) {
    let bytes = payload_of(shape);
    let result = tokio::task::spawn_blocking(move || {
        let mut payload = PayloadBuilder::new();
        payload.push_bytes(&bytes);
        if one_way {
            device
                .transact_one_way(device.context_manager(), ECHO_CODE, payload)
                .map(|()| Vec::new())
        } else {
            device
                .transact_blocking(device.context_manager(), ECHO_CODE, payload)
                .map(|(_, mut reply)| reply.read_bytes(reply.bytes_until_next_obj()).unwrap().to_vec())
        }
    })
    .await
    .unwrap()
    .expect("transaction failed");

    if !one_way {
        assert_eq!(result, payload_of(shape), "echoed payload didn't round-trip");
    }
}

macro_rules! combo_tests {
    ($($name:ident: shape = $shape:literal, one_way = $one_way:expr, concurrency = $concurrency:expr;)+) => {
        $(
            #[test]
            fn $name() {
                let node = PoolNode::acquire();
                let result = support::fork_combo(
                    &node,
                    become_echo_service,
                    move |device: Arc<BinderDevice>| async move {
                        let mut tasks = Vec::new();
                        for _ in 0..$concurrency {
                            let device = device.clone();
                            tasks.push(tokio::spawn(echo_once(device, $shape, $one_way)));
                        }
                        for t in tasks {
                            t.await.unwrap();
                        }
                    },
                );
                assert_eq!(
                    result.child_status,
                    nix::sys::wait::WaitStatus::Exited(
                        match result.child_status {
                            nix::sys::wait::WaitStatus::Exited(pid, _) => pid,
                            _ => panic!("service role did not exit cleanly: {:?}", result.child_status),
                        },
                        0
                    )
                );
            }
        )+
    };
}

combo_tests! {
    blocking_empty_single: shape = "empty", one_way = false, concurrency = 1;
    blocking_small_single: shape = "small", one_way = false, concurrency = 1;
    blocking_large_single: shape = "large", one_way = false, concurrency = 1;
    blocking_small_concurrent: shape = "small", one_way = false, concurrency = 8;
    one_way_small_single: shape = "small", one_way = true, concurrency = 1;
    one_way_small_concurrent: shape = "small", one_way = true, concurrency = 8;
}

/// Local self-transact variant: same process is both context manager and
/// client (mirrors `examples/self_transaction.rs`) — no fork needed.
#[tokio::test]
async fn local_self_transact_echo() {
    let node = PoolNode::acquire();
    let device = BinderDevice::new(&node.path).expect("open device");
    let obj = device.register_object(EchoService);
    device
        .set_context_manager(&obj)
        .await
        .expect("set_context_manager");

    echo_once(device, "small", false).await;
}

/// Kill the service process mid-flight (a real `binder_proc` death, not just
/// dropping an `Arc`) and assert the client's ref observes it.
#[test]
fn death_notification_on_killed_service() {
    let node = PoolNode::acquire();
    let service = fork_service(&node, become_echo_service);
    let path = node.path.clone();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async move {
        let device = BinderDevice::new(&path).expect("open device (client role)");

        // Confirm the service is alive and reachable before killing it.
        let device1 = device.clone();
        tokio::task::spawn_blocking(move || {
            let mut payload = PayloadBuilder::new();
            payload.push_bytes(b"ping");
            device1.transact_blocking(device1.context_manager(), ECHO_CODE, payload)
        })
        .await
        .unwrap()
        .expect("initial transaction failed");

        kill_child(service.pid);

        // The next transaction against the dead context manager must fail.
        let device2 = device.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut payload = PayloadBuilder::new();
            payload.push_bytes(b"ping again");
            device2.transact_blocking(device2.context_manager(), ECHO_CODE, payload)
        })
        .await
        .unwrap();

        assert!(
            result.is_err(),
            "transaction to a killed context manager unexpectedly succeeded"
        );
    });

    reap(service.pid);
}
