use binderbinder::{
    binder_ports::{BinderObjectOrRef, BinderRef},
    payload::PayloadBuilder,
    BinderDevice,
};
use tokio::task::spawn_blocking;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

const ECHO_CODE: u32 = 1;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    info!("Opening binder device...");

    let device = BinderDevice::new("/dev/binderfs/testbinder")?;
    info!("Binder opened");

    info!("Sending transaction to context manager");
    let transaction_future = spawn_blocking(move || {
        let mut payload = PayloadBuilder::new();
        payload.push_bytes(b"hello from self echoed by the context manager");
        device.transact_blocking(
            &BinderObjectOrRef::Ref(BinderRef::get_context_manager_handle(&device)),
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

    Ok(())
}
