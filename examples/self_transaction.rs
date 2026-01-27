use binderbinder::{BinderDevice, BinderRef, Payload, Transaction, TransactionHandler};

const ECHO_CODE: u32 = 1;

pub struct BinderObject;
impl TransactionHandler for BinderObject {
    async fn handle(&self, transaction: Transaction) -> Payload {
        transaction.payload
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Opening binder device...");
    let file = std::fs::File::open("/dev/binderfs/testbinder")?;

    let device = BinderDevice::new(file);
    eprintln!("Binder opened");

    eprintln!("Setting context manager...");
    let obj = device.register_object(BinderObject);
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
