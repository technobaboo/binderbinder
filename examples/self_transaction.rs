use binderbinder::{BinderDevice, BinderRef, Payload};

extern crate libc;

const ECHO_CODE: u32 = 1;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Opening binder device...");
    let file = std::fs::File::open("/dev/binderfs/testbinder")?;

    let device = BinderDevice::open(file).await?;
    eprintln!("Binder opened");

    eprintln!("Setting context manager...");
    let obj = device.register_object();
    let _cm_ref = device.set_context_manager(obj).await?;
    eprintln!("Context manager set!");

    eprintln!("Sending transaction to self (sync)...");
    let payload = Payload::with_data(b"hello from self".to_vec());
    match device.transact(BinderRef(0), ECHO_CODE, payload).await {
        Ok(reply) => {
            eprintln!("Received: {:?}", String::from_utf8_lossy(&reply.data));
        }
        Err(e) => {
            eprintln!("Error: {:?}", e);
        }
    }

    Ok(())
}
