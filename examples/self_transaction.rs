use binderbinder::{
    BinderDevice, TransactionHandler, binder_ports::{BinderPort, BinderPortHandle}, device::Transaction, payload::{BinderObjectType, PayloadBuilder}
};
use tokio::task::spawn_blocking;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

const ECHO_CODE: u32 = 1;

pub struct BinderObject;
impl TransactionHandler for BinderObject {
    async fn handle(&self, mut transaction: Transaction) -> PayloadBuilder<'_> {
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

    async fn handle_one_way(&self, _transaction: binderbinder::device::Transaction) {
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

    info!("Setting context manager...");
    let obj = device.register_object(BinderObject);
    let _cm_ref = device.set_context_manager(&obj).await?;
    info!("Context manager set!");

    info!("Sending echo transaction to self");
    let transaction_future = spawn_blocking(move || {
        let mut payload = PayloadBuilder::new();
        payload.push_bytes(b"hello from self");
        device.transact_blocking(
            &BinderPort::Handle(BinderPortHandle::get_context_manager_handle(&device)),
            ECHO_CODE,
            payload,
        )
    });
    match transaction_future.await.unwrap() {
        Ok((_, mut reply)) => {
            info!(
                "Received: {:?}",
                String::from_utf8_lossy(&reply.read_bytes(reply.bytes_until_next_obj()).unwrap())
            );
        }
        Err(e) => {
            error!("Error: {:?}", e);
        }
    }

    tokio::signal::ctrl_c().await.unwrap();
    Ok(())
}
