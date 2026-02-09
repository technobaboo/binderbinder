use std::time::Duration;

use binderbinder::{
    binder_ports::BinderPortHandle,
    device::Transaction,
    transaction_data::{BinderObjectType, PayloadBuilder},
    BinderDevice, BinderRef, TransactionHandler,
};
use tokio::{task::spawn_blocking, time::sleep};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

const ECHO_CODE: u32 = 1;

pub struct BinderObject;
impl TransactionHandler for BinderObject {
    async fn handle(&self, mut transaction: Transaction) -> PayloadBuilder {
        let mut builder = PayloadBuilder::new();
        if transaction.code != ECHO_CODE {
            builder.push_bytes(b"unknown transaction code");
            return builder;
        }
        loop {
            let bytes = transaction.payload.bytes_until_next_obj();
            if bytes != 0 {
                let Ok(v) = transaction
                    .payload
                    .read_bytes(bytes)
                    .inspect_err(|err| error!("failed to read bytes: {err}"))
                else {
                    break;
                };
                builder.push_bytes(v);
                continue;
            }
            match transaction.payload.next_object_type() {
                Some(BinderObjectType::PortHandle)
                | Some(BinderObjectType::WeakPortHandle)
                | Some(BinderObjectType::WeakOwnedPort)
                | Some(BinderObjectType::OwnedPort) => {
                    builder.push_port(&transaction.payload.read_port().unwrap());
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

    async fn handle_one_way(&self, transaction: binderbinder::device::Transaction) {
        info!("got oneway transaction")
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    info!("Opening binder device...");

    let device = BinderDevice::new("/dev/binderfs/testbinder")?;
    info!("Binder opened");
    sleep(Duration::from_secs(1)).await;
    info!("Setting context manager...");
    let obj = device.register_object(BinderObject);
    let _cm_ref = device.set_context_manager(&obj).await?;
    info!("Context manager set!");

    tokio::signal::ctrl_c().await.unwrap();
    Ok(())
}
