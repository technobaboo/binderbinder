use binderbinder::{BinderDevice, BinderRef, Payload};

extern crate libc;

const ECHO_CODE: u32 = 1;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Opening binder device...");
    let file = std::fs::File::open("/dev/binderfs/testbinder")?;

    match unsafe { libc::fork() } {
        -1 => panic!("fork failed"),
        0 => {
            let device = BinderDevice::open(file).await?;
            eprintln!("Parent: device created (sync mode)");

            eprintln!("Parent: waiting 500ms for parent to be ready...");
            std::thread::sleep(std::time::Duration::from_millis(500));

            eprintln!("Parent: sending transaction to context manager (handle 0)...");
            let payload = Payload::with_data(b"hello from child".to_vec());
            match device.transact(BinderRef(0), ECHO_CODE, payload).await {
                Ok(reply) => {
                    eprintln!(
                        "Parent received: {:?}",
                        String::from_utf8_lossy(&reply.data)
                    );
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("Parent error: {:?}", e);
                    std::process::exit(1);
                }
            }
        }
        pid => {
            let device = BinderDevice::open(file).await?;
            eprintln!("Child: binder opened");

            eprintln!("Child: setting context manager...");
            let obj = device.register_object();
            let _cm_ref = device.set_context_manager(obj).await?;
            eprintln!("Child: context manager set!");
            eprintln!("Child: waiting for transaction...");
            let mut listener = device.register_object();
            if let Some(tx) = listener.recv_transaction().await {
                let data = tx.payload.data.clone();
                eprintln!("Child received: {:?}", String::from_utf8_lossy(&data));
                tx.reply(Payload::with_data(data));
                eprintln!("Child: reply sent!");
            } else {
                eprintln!("Child: no transaction received");
            }

            eprintln!("Child: waiting for parent...");
            let _ = unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
            eprintln!("Child: parent exited");
        }
    }

    Ok(())
}
